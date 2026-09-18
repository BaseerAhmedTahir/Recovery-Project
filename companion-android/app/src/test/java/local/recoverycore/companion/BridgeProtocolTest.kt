package local.recoverycore.companion

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.BufferedReader
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.StringReader

/**
 * The phone's half of the bridge protocol, and the bytes it produces.
 *
 * The session this writes is saved as `protocol-golden.bin` in the project
 * root; `crates/rc-mobile/tests/bridge.rs` replays that exact file into the
 * desktop receiver and checks the files arrive. So the two implementations are
 * checked against one recorded conversation rather than against each other's
 * assumptions, without a phone or an emulator. If this test changes the bytes,
 * the Rust test fails until the golden file is regenerated deliberately.
 */
class BridgeProtocolTest {

    private fun session(): ByteArray {
        val photo = ByteArray(5000) { (it * 7 % 251).toByte() }
        val note = "a recovered note\n".toByteArray()
        val items = listOf(
            Bridge.Item(
                path = "/storage/emulated/0/DCIM/Camera/.trashed-1726000000-IMG_1.jpg",
                category = "trashed",
                size = photo.size.toLong(),
                open = { ByteArrayInputStream(photo) },
            ),
            Bridge.Item(
                path = "/storage/emulated/0/Documents/note \"quoted\".txt",
                category = "app-media",
                size = note.size.toLong(),
                open = { ByteArrayInputStream(note) },
            ),
        )
        val out = ByteArrayOutputStream()
        // What the computer says back, in order.
        val replies = StringReader("OK\nACK a\nACK b\nBYE 2\n")
        val results = Bridge.writeSession(out, BufferedReader(replies), "123456", items)
        assertEquals(2, results.size)
        assertTrue(results.all { it.accepted })
        return out.toByteArray()
    }

    @Test
    fun the_session_starts_with_the_protocol_line_and_ends_with_end() {
        val text = String(session(), Charsets.ISO_8859_1)
        assertTrue(text.startsWith("RCB1 123456\n"))
        assertTrue(text.endsWith("{\"end\":true}\n"))
        // The header of the first file, with its real SHA-256.
        val sha = Bridge.sha256(ByteArrayInputStream(ByteArray(5000) { (it * 7 % 251).toByte() }))
        assertTrue(text.contains("\"size\":5000,\"sha256\":\"$sha\",\"category\":\"trashed\""))
        // A quote in a name is escaped, not left to break the header.
        assertTrue(text.contains("note \\\"quoted\\\".txt"))
    }

    @Test
    fun a_wrong_code_is_refused_rather_than_sending_anything() {
        val out = ByteArrayOutputStream()
        val e = runCatching {
            Bridge.writeSession(out, BufferedReader(StringReader("NO\n")), "000000", emptyList())
        }.exceptionOrNull()
        assertTrue(e is Bridge.Refused)
        assertEquals("RCB1 000000\n", String(out.toByteArray(), Charsets.ISO_8859_1))
    }

    @Test
    fun the_golden_session_matches_the_one_the_desktop_replays() {
        val bytes = session()
        // The project root when Gradle runs tests is companion-android/app.
        val golden = File("../protocol-golden.bin")
        if (!golden.exists() || System.getenv("RC_WRITE_GOLDEN") == "1") {
            golden.writeBytes(bytes)
        }
        assertTrue(
            "protocol-golden.bin no longer matches what this app sends. If the change is " +
                "deliberate, regenerate it with RC_WRITE_GOLDEN=1 and update the Rust test.",
            golden.readBytes().contentEquals(bytes),
        )
    }

    @Test
    fun only_loopback_is_allowed() {
        val e = runCatching { Bridge.connect("10.0.0.5", 38300) }.exceptionOrNull()
        assertTrue("connecting to a LAN address must be refused: $e", e is Bridge.Refused)
    }
}
