package local.recoverycore.companion

import android.app.Activity
import android.content.ContentResolver
import android.content.ContentUris
import android.content.Context
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.MediaStore
import java.io.File
import java.io.InputStream

/**
 * What this phone will let an ordinary app recover (SPEC.md 1.2, 6.4).
 *
 * - **The system trash** (Android 11+): photos and videos deleted in the last
 *   ~30 days are still in MediaStore with `is_trashed = 1`. Listing them needs
 *   `QUERY_ARG_MATCH_TRASHED`; putting one back is
 *   `MediaStore.createTrashRequest`, which shows the system's own dialog, so
 *   the person holding the phone approves every restore.
 * - **Media caches**: thumbnails and messaging-app media directories on shared
 *   storage, which often hold a smaller copy of a photo whose original is gone.
 *
 * Nothing else is possible without root: an app cannot read another app's
 * private files, and deleted blocks are encrypted per file and erased by TRIM.
 */
object Media {

    data class Item(
        val uri: Uri?,
        val displayName: String,
        val relativePath: String,
        val size: Long,
        val mimeType: String?,
        val dateMillis: Long,
        /** Trashed items: when the system will delete it for good. */
        val expiresMillis: Long?,
        val source: Source,
        /** For files found on disk rather than through MediaStore. */
        val file: File? = null,
    ) {
        val isTrashed get() = source == Source.TRASH
        fun open(resolver: ContentResolver): InputStream =
            file?.inputStream() ?: resolver.openInputStream(uri!!)!!
    }

    enum class Source { TRASH, TRASHED_FILE, THUMBNAIL, APP_MEDIA }

    val trashSupported: Boolean get() = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R

    /** Photos and videos in the system trash. Empty below Android 11. */
    fun trashed(context: Context): List<Item> {
        if (!trashSupported) return emptyList()
        val resolver = context.contentResolver
        val out = ArrayList<Item>()
        val projection = arrayOf(
            MediaStore.MediaColumns._ID,
            MediaStore.MediaColumns.DISPLAY_NAME,
            MediaStore.MediaColumns.RELATIVE_PATH,
            MediaStore.MediaColumns.SIZE,
            MediaStore.MediaColumns.MIME_TYPE,
            MediaStore.MediaColumns.DATE_MODIFIED,
            MediaStore.MediaColumns.DATE_EXPIRES,
        )
        val args = Bundle().apply {
            putInt(MediaStore.QUERY_ARG_MATCH_TRASHED, MediaStore.MATCH_ONLY)
            putString(
                ContentResolver.QUERY_ARG_SQL_SORT_ORDER,
                "${MediaStore.MediaColumns.DATE_EXPIRES} ASC",
            )
        }
        for (collection in listOf(
            MediaStore.Images.Media.getContentUri(MediaStore.VOLUME_EXTERNAL),
            MediaStore.Video.Media.getContentUri(MediaStore.VOLUME_EXTERNAL),
        )) {
            resolver.query(collection, projection, args, null)?.use { c ->
                val id = c.getColumnIndexOrThrow(MediaStore.MediaColumns._ID)
                val name = c.getColumnIndexOrThrow(MediaStore.MediaColumns.DISPLAY_NAME)
                val path = c.getColumnIndexOrThrow(MediaStore.MediaColumns.RELATIVE_PATH)
                val size = c.getColumnIndexOrThrow(MediaStore.MediaColumns.SIZE)
                val mime = c.getColumnIndexOrThrow(MediaStore.MediaColumns.MIME_TYPE)
                val date = c.getColumnIndexOrThrow(MediaStore.MediaColumns.DATE_MODIFIED)
                val expires = c.getColumnIndexOrThrow(MediaStore.MediaColumns.DATE_EXPIRES)
                while (c.moveToNext()) {
                    out.add(
                        Item(
                            uri = ContentUris.withAppendedId(collection, c.getLong(id)),
                            displayName = c.getString(name) ?: "(no name)",
                            relativePath = c.getString(path) ?: "",
                            size = c.getLong(size),
                            mimeType = c.getString(mime),
                            dateMillis = c.getLong(date) * 1000,
                            expiresMillis = c.getLong(expires).takeIf { it > 0 }?.times(1000),
                            source = Source.TRASH,
                        )
                    )
                }
            }
        }
        return out
    }

    /**
     * Ask the system to put [items] back. The system shows its own
     * confirmation; this returns the request for the caller to launch.
     */
    fun untrashRequest(context: Context, items: List<Item>) =
        MediaStore.createTrashRequest(
            context.contentResolver,
            items.mapNotNull { it.uri },
            false,
        )

    /**
     * Files on shared storage that are worth sending to the computer even
     * though they are not in the trash: thumbnails, messaging-app media, and
     * `.trashed-*` files the media store has forgotten.
     */
    fun caches(context: Context): List<Item> {
        val roots = listOf(
            "DCIM/.thumbnails" to Source.THUMBNAIL,
            "Pictures/.thumbnails" to Source.THUMBNAIL,
            "Android/media/com.whatsapp/WhatsApp/Media" to Source.APP_MEDIA,
            "Android/media/org.telegram.messenger" to Source.APP_MEDIA,
            "Telegram" to Source.APP_MEDIA,
        )
        val base = android.os.Environment.getExternalStorageDirectory()
        val out = ArrayList<Item>()
        for ((rel, source) in roots) {
            walk(File(base, rel), source, out)
        }
        walkTrashedNames(base, out)
        return out.sortedByDescending { it.dateMillis }
    }

    private fun walk(dir: File, source: Source, out: MutableList<Item>, depth: Int = 0) {
        if (depth > 6 || !dir.isDirectory) return
        val entries = dir.listFiles() ?: return
        for (f in entries) {
            if (f.isDirectory) {
                walk(f, source, out, depth + 1)
            } else if (f.length() > 0 && out.size < 20_000) {
                out.add(
                    Item(
                        uri = null,
                        displayName = f.name,
                        relativePath = f.parent ?: "",
                        size = f.length(),
                        mimeType = null,
                        dateMillis = f.lastModified(),
                        expiresMillis = null,
                        source = source,
                        file = f,
                    )
                )
            }
        }
    }

    /** `.trashed-<expiry>-<name>` files still on disk. */
    private fun walkTrashedNames(base: File, out: MutableList<Item>, dir: File = base, depth: Int = 0) {
        if (depth > 5 || !dir.isDirectory) return
        val entries = dir.listFiles() ?: return
        for (f in entries) {
            if (f.isDirectory) {
                walkTrashedNames(base, out, f, depth + 1)
            } else if (f.name.startsWith(".trashed-")) {
                val parts = f.name.removePrefix(".trashed-").split("-", limit = 2)
                out.add(
                    Item(
                        uri = null,
                        displayName = parts.getOrNull(1) ?: f.name,
                        relativePath = f.parent ?: "",
                        size = f.length(),
                        mimeType = null,
                        dateMillis = f.lastModified(),
                        expiresMillis = parts.getOrNull(0)?.toLongOrNull()?.times(1000),
                        source = Source.TRASHED_FILE,
                        file = f,
                    )
                )
            }
        }
    }

    /** The permission this Android version needs to list media. */
    fun requiredPermissions(): Array<String> =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            arrayOf(
                android.Manifest.permission.READ_MEDIA_IMAGES,
                android.Manifest.permission.READ_MEDIA_VIDEO,
            )
        } else {
            arrayOf(android.Manifest.permission.READ_EXTERNAL_STORAGE)
        }

    fun hasPermission(activity: Activity): Boolean = requiredPermissions().all {
        activity.checkSelfPermission(it) == android.content.pm.PackageManager.PERMISSION_GRANTED
    }
}
