package local.recoverycore.companion

import java.io.BufferedReader
import java.io.InputStream
import java.io.OutputStream
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Socket
import java.security.MessageDigest

/**
 * The phone end of the RCB1 bridge (docs/BRIDGE.md).
 *
 * It connects to 127.0.0.1 only. The computer runs `adb reverse tcp:PORT
 * tcp:PORT`, which carries that loopback connection down the USB cable; no
 * packet leaves the phone by any other route, and [connect] refuses an address
 * that is not loopback rather than trusting the caller.
 *
 * The protocol is written here exactly as the desktop reads it, and
 * [writeSession] is exercised by a unit test that produces the same bytes the
 * Rust receiver's test replays, so the two ends are checked against one
 * description rather than against each other's assumptions.
 */
object Bridge {
    const val PROTOCOL = "RCB1"

    class Refused(message: String) : Exception(message)

    /** One file to send: where to read it from, and what to call it. */
    data class Item(
        val path: String,
        val category: String,
        val size: Long,
        val open: () -> InputStream,
    )

    /** What the computer said about one file. */
    data class Result(val path: String, val accepted: Boolean, val reply: String)

    fun sha256(stream: InputStream): String {
        val digest = MessageDigest.getInstance("SHA-256")
        val buf = ByteArray(1 shl 16)
        stream.use {
            while (true) {
                val n = it.read(buf)
                if (n <= 0) break
                digest.update(buf, 0, n)
            }
        }
        return digest.digest().joinToString("") { "%02x".format(it) }
    }

    fun connect(host: String, port: Int, timeoutMs: Int = 10_000): Socket {
        val address = InetAddress.getByName(host)
        if (!address.isLoopbackAddress) {
            throw Refused(
                "the bridge only connects to this phone's own loopback address; " +
                    "the computer is reached through the USB cable (adb reverse)"
            )
        }
        val socket = Socket()
        socket.connect(InetSocketAddress(address, port), timeoutMs)
        socket.soTimeout = timeoutMs
        return socket
    }

    /**
     * Send [items] to a paired computer. Returns one result per file. Throws
     * [Refused] if the pairing code is wrong.
     */
    fun writeSession(
        out: OutputStream,
        input: BufferedReader,
        code: String,
        items: List<Item>,
        onProgress: (index: Int, sentBytes: Long) -> Unit = { _, _ -> },
    ): List<Result> {
        out.write("$PROTOCOL $code\n".toByteArray())
        out.flush()
        val hello = input.readLine() ?: throw Refused("the computer closed the connection")
        if (hello.trim() != "OK") {
            throw Refused("the computer did not accept the pairing code (it said \"$hello\")")
        }
        val results = ArrayList<Result>(items.size)
        items.forEachIndexed { index, item ->
            val sha = sha256(item.open())
            val header = buildString {
                append('{')
                append("\"path\":").append(jsonString(item.path)).append(',')
                append("\"size\":").append(item.size).append(',')
                append("\"sha256\":").append(jsonString(sha)).append(',')
                append("\"category\":").append(jsonString(item.category))
                append('}')
            }
            out.write("$header\n".toByteArray())
            var sent = 0L
            item.open().use { stream ->
                val buf = ByteArray(1 shl 16)
                while (sent < item.size) {
                    val n = stream.read(buf, 0, minOf(buf.size.toLong(), item.size - sent).toInt())
                    if (n <= 0) break
                    out.write(buf, 0, n)
                    sent += n
                    onProgress(index, sent)
                }
            }
            // A file that turned out shorter than its recorded size still has to
            // fill the frame the computer is reading, or everything after it
            // would be misread.
            if (sent < item.size) {
                val pad = ByteArray(1 shl 16)
                var left = item.size - sent
                while (left > 0) {
                    val n = minOf(left, pad.size.toLong()).toInt()
                    out.write(pad, 0, n)
                    left -= n
                }
            }
            out.flush()
            val reply = input.readLine() ?: throw Refused("the computer closed the connection")
            results.add(Result(item.path, reply.startsWith("ACK"), reply))
        }
        out.write("{\"end\":true}\n".toByteArray())
        out.flush()
        input.readLine()
        return results
    }

    fun jsonString(s: String): String {
        val sb = StringBuilder("\"")
        for (c in s) {
            when {
                c == '"' -> sb.append("\\\"")
                c == '\\' -> sb.append("\\\\")
                c == '\n' -> sb.append("\\n")
                c == '\r' -> sb.append("\\r")
                c == '\t' -> sb.append("\\t")
                c < ' ' -> sb.append("\\u%04x".format(c.code))
                else -> sb.append(c)
            }
        }
        return sb.append('"').toString()
    }
}
