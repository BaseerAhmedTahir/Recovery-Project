package local.recoverycore.companion

import android.app.Activity
import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.IntentSenderRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import java.io.BufferedReader
import java.io.InputStreamReader
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import kotlin.concurrent.thread

/**
 * The companion app. Three things, all of them things an unrooted phone can
 * actually do, and nothing hidden: the app has no background component and
 * does nothing unless it is open.
 *
 * 1. List what is in the system trash and put items back (the system asks for
 *    confirmation itself).
 * 2. List media caches and `.trashed-*` files still on disk.
 * 3. Send anything on those lists to the desktop tool over the USB cable.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent { MaterialTheme { App(this) } }
    }
}

private fun fmtSize(n: Long): String {
    val u = listOf("B", "KB", "MB", "GB")
    var v = n.toDouble()
    var i = 0
    while (v >= 1024 && i < u.size - 1) { v /= 1024; i++ }
    return if (i == 0) "$n B" else String.format(Locale.US, "%.1f %s", v, u[i])
}

private fun fmtDate(ms: Long): String =
    SimpleDateFormat("yyyy-MM-dd HH:mm", Locale.US).format(Date(ms))

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun App(activity: Activity) {
    var tab by remember { mutableIntStateOf(0) }
    var items by remember { mutableStateOf<List<Media.Item>>(emptyList()) }
    var selected by remember { mutableStateOf<Set<String>>(emptySet()) }
    var status by remember { mutableStateOf("") }
    var granted by remember { mutableStateOf(Media.hasPermission(activity)) }
    var port by remember { mutableStateOf("38300") }
    var code by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }

    val ask = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        granted = Media.hasPermission(activity)
    }
    val untrash = rememberLauncherForActivityResult(ActivityResultContracts.StartIntentSenderForResult()) { r ->
        status = if (r.resultCode == Activity.RESULT_OK) {
            "Put back. They are in their original folders again."
        } else {
            "Nothing was restored (the request was declined)."
        }
    }

    fun load() {
        status = "Looking…"
        thread {
            val found = if (tab == 0) Media.trashed(activity) else Media.caches(activity)
            activity.runOnUiThread {
                items = found
                selected = emptySet()
                status = when {
                    tab == 0 && !Media.trashSupported ->
                        "This phone runs Android ${Build.VERSION.RELEASE}. The system trash " +
                            "arrived in Android 11, so there is none to read here. The other " +
                            "tab still finds caches and leftover files."
                    found.isEmpty() -> "Nothing found."
                    else -> "${found.size} items."
                }
            }
        }
    }

    LaunchedEffect(tab, granted) { if (granted) load() }

    fun key(i: Media.Item) = i.uri?.toString() ?: i.file?.absolutePath ?: i.displayName

    Column(Modifier.fillMaxSize().padding(12.dp)) {
        Text("RECOVERY Companion", style = MaterialTheme.typography.headlineSmall)
        Text(
            "Recovers what an app on this phone is allowed to reach. Deleted photos that " +
                "are past the trash are gone from the phone itself - use the computer tool on " +
                "the memory card or on a backup.",
            style = MaterialTheme.typography.bodySmall,
        )
        Spacer(Modifier.height(8.dp))

        if (!granted) {
            Card(Modifier.fillMaxWidth()) {
                Column(Modifier.padding(12.dp)) {
                    Text("This app needs permission to read photos and videos to list what can be recovered.")
                    Spacer(Modifier.height(8.dp))
                    Button(onClick = { ask.launch(Media.requiredPermissions()) }) { Text("Grant permission") }
                }
            }
            return@Column
        }

        TabRow(selectedTabIndex = tab) {
            Tab(tab == 0, { tab = 0 }, text = { Text("Trash") })
            Tab(tab == 1, { tab = 1 }, text = { Text("Caches & leftovers") })
        }
        Spacer(Modifier.height(8.dp))
        Row(verticalAlignment = Alignment.CenterVertically) {
            Button(onClick = { load() }, enabled = !busy) { Text("Refresh") }
            Spacer(Modifier.width(8.dp))
            TextButton(onClick = { selected = items.map { key(it) }.toSet() }) { Text("Select all") }
            TextButton(onClick = { selected = emptySet() }) { Text("None") }
            Spacer(Modifier.weight(1f))
            Text("${selected.size} selected", style = MaterialTheme.typography.bodySmall)
        }
        Text(status, style = MaterialTheme.typography.bodySmall)

        LazyColumn(Modifier.weight(1f)) {
            items(items, key = { key(it) }) { item ->
                val k = key(item)
                ListItem(
                    headlineContent = { Text(item.displayName, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                    supportingContent = {
                        val expiry = item.expiresMillis?.let { " · deleted for good ${fmtDate(it)}" } ?: ""
                        Text(
                            "${fmtSize(item.size)} · ${fmtDate(item.dateMillis)}$expiry",
                            style = MaterialTheme.typography.bodySmall,
                        )
                    },
                    leadingContent = {
                        Checkbox(
                            checked = selected.contains(k),
                            onCheckedChange = {
                                selected = if (it) selected + k else selected - k
                            },
                        )
                    },
                )
                HorizontalDivider()
            }
        }

        val chosen = items.filter { selected.contains(key(it)) }
        if (tab == 0 && Media.trashSupported) {
            Button(
                enabled = chosen.isNotEmpty() && !busy,
                onClick = {
                    val req = Media.untrashRequest(activity, chosen)
                    untrash.launch(IntentSenderRequest.Builder(req.intentSender).build())
                },
                modifier = Modifier.fillMaxWidth(),
            ) { Text("Put ${chosen.size} back on the phone") }
        }

        HorizontalDivider(Modifier.padding(vertical = 8.dp))
        Text("Send to the computer over USB", style = MaterialTheme.typography.titleSmall)
        Text(
            "On the computer run: rc bridge --out <folder>. Plug in the USB cable, then type " +
                "the six-digit code it shows.",
            style = MaterialTheme.typography.bodySmall,
        )
        Row(verticalAlignment = Alignment.CenterVertically) {
            OutlinedTextField(
                value = code,
                onValueChange = { code = it.filter { c -> c.isDigit() }.take(6) },
                label = { Text("Code") },
                modifier = Modifier.width(120.dp),
                singleLine = true,
            )
            Spacer(Modifier.width(8.dp))
            OutlinedTextField(
                value = port,
                onValueChange = { port = it.filter { c -> c.isDigit() }.take(5) },
                label = { Text("Port") },
                modifier = Modifier.width(110.dp),
                singleLine = true,
            )
            Spacer(Modifier.width(8.dp))
            Button(
                enabled = chosen.isNotEmpty() && code.length == 6 && !busy,
                onClick = {
                    busy = true
                    status = "Sending ${chosen.size} files…"
                    thread {
                        val message = send(activity, chosen, code, port.toIntOrNull() ?: 38300)
                        activity.runOnUiThread {
                            status = message
                            busy = false
                        }
                    }
                },
            ) { Text("Send ${chosen.size}") }
        }
        Spacer(Modifier.height(8.dp))
    }
}

private fun send(activity: Activity, chosen: List<Media.Item>, code: String, port: Int): String =
    try {
        val resolver = activity.contentResolver
        val items = chosen.map { item ->
            Bridge.Item(
                path = (item.file?.absolutePath ?: "${item.relativePath}/${item.displayName}"),
                category = when (item.source) {
                    Media.Source.TRASH, Media.Source.TRASHED_FILE -> "trashed"
                    Media.Source.THUMBNAIL -> "thumbnail"
                    Media.Source.APP_MEDIA -> "app-media"
                },
                size = item.size,
                open = { item.open(resolver) },
            )
        }
        Bridge.connect("127.0.0.1", port).use { socket ->
            val reader = BufferedReader(InputStreamReader(socket.getInputStream()))
            val results = Bridge.writeSession(socket.getOutputStream(), reader, code, items)
            val ok = results.count { it.accepted }
            "Sent $ok of ${results.size} files." +
                results.firstOrNull { !it.accepted }?.let { " First refusal: ${it.reply}" }.orEmpty()
        }
    } catch (e: Bridge.Refused) {
        e.message ?: "Refused."
    } catch (e: Exception) {
        "Could not reach the computer: ${e.message}. Check the USB cable and that " +
            "`rc bridge` is running (it sets up the tunnel with adb reverse)."
    }
