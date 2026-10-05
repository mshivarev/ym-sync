package dev.mshiv.ymsync

import android.content.Context
import android.content.Intent
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.Settings
import androidx.core.content.FileProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.File
import java.net.HttpURLConnection
import java.net.URL

/** A release on GitHub newer than what is installed. */
data class Release(
    val version: String,
    /** The release notes, as written on GitHub. */
    val notes: String,
    val apkUrl: String,
)

/**
 * Updates from GitHub Releases.
 *
 * Android lets an app outside Google Play download its own update, but not install
 * it unasked: the system installer always shows its dialog, and the first time it
 * also asks for permission to install from this app. What protects the update is
 * the APK's own signature — Android refuses to replace an app with one signed by a
 * different key — and [install] checks that up front, so a wrong file is reported
 * in words instead of as the installer's «conflicts with an existing package».
 */
object Updater {
    private const val LATEST = "https://api.github.com/repos/mshivarev/ym-sync/releases/latest"
    private const val APK_SUFFIX = "-android.apk"

    /** What is installed, as `0.2.0`. */
    fun currentVersion(context: Context): String =
        runCatching {
            context.packageManager.getPackageInfo(context.packageName, 0).versionName
        }.getOrNull() ?: "0"

    /**
     * The latest release, when it is newer than this one.
     *
     * Null when it is not — and also when GitHub could not be asked, or has no
     * release yet: neither is worth a message on every launch.
     */
    suspend fun latest(context: Context): Release? = withContext(Dispatchers.IO) {
        runCatching {
            val connection = URL(LATEST).openConnection() as HttpURLConnection
            connection.connectTimeout = 10_000
            connection.readTimeout = 10_000
            connection.setRequestProperty("Accept", "application/vnd.github+json")
            val body = try {
                if (connection.responseCode != 200) return@runCatching null
                connection.inputStream.use { it.readBytes().decodeToString() }
            } finally {
                connection.disconnect()
            }
            val release = JSONObject(body)
            val version = release.optString("tag_name").removePrefix("v").trim()
            if (version.isEmpty() || !isNewer(version, currentVersion(context))) {
                return@runCatching null
            }
            val assets = release.optJSONArray("assets") ?: return@runCatching null
            val apk = (0 until assets.length())
                .mapNotNull { assets.optJSONObject(it) }
                .firstOrNull { it.optString("name").endsWith(APK_SUFFIX) }
                ?: return@runCatching null
            Release(
                version = version,
                notes = release.optString("body"),
                apkUrl = apk.optString("browser_download_url"),
            )
        }.getOrNull()
    }

    /** `0.10.0` is newer than `0.9.3`: the parts are numbers, not text. */
    fun isNewer(candidate: String, current: String): Boolean {
        val a = candidate.split('.', '-').map { it.toIntOrNull() ?: 0 }
        val b = current.split('.', '-').map { it.toIntOrNull() ?: 0 }
        for (i in 0 until maxOf(a.size, b.size)) {
            val x = a.getOrElse(i) { 0 }
            val y = b.getOrElse(i) { 0 }
            if (x != y) return x > y
        }
        return false
    }

    /**
     * Downloads the APK into the app's own cache, reporting progress from 0 to 1.
     *
     * The cache rather than shared storage: nothing else needs to read it, and no
     * storage permission is involved. Earlier downloads are cleared first.
     */
    suspend fun download(
        context: Context,
        release: Release,
        onProgress: (Float) -> Unit,
    ): File = withContext(Dispatchers.IO) {
        val folder = File(context.cacheDir, "updates").apply {
            deleteRecursively()
            mkdirs()
        }
        val target = File(folder, "ymsync-${release.version}.apk")

        val connection = URL(release.apkUrl).openConnection() as HttpURLConnection
        connection.connectTimeout = 15_000
        connection.readTimeout = 30_000
        // GitHub answers with a redirect to its file storage.
        connection.instanceFollowRedirects = true
        try {
            if (connection.responseCode != 200) {
                error("GitHub ответил ${connection.responseCode}")
            }
            val total = connection.contentLengthLong
            connection.inputStream.use { input ->
                target.outputStream().use { output ->
                    val buffer = ByteArray(64 * 1024)
                    var done = 0L
                    while (true) {
                        val read = input.read(buffer)
                        if (read < 0) break
                        output.write(buffer, 0, read)
                        done += read
                        if (total > 0) onProgress(done.toFloat() / total)
                    }
                }
            }
        } finally {
            connection.disconnect()
        }
        target
    }

    /**
     * Hands the downloaded APK to the system installer.
     *
     * Returns a message when it cannot: the file is not this app or is signed by
     * someone else, or the user has yet to allow installs from this app — in which
     * case the settings page for that is opened and the button can be pressed
     * again afterwards.
     */
    fun install(context: Context, apk: File): String? {
        val pm = context.packageManager
        val archive = archiveInfo(pm, apk) ?: return "это не установочный файл Android"
        if (archive.packageName != context.packageName) {
            return "файл от другого приложения: ${archive.packageName}"
        }
        if (!sameSigner(pm, context.packageName, archive)) {
            return "файл подписан чужим ключом — устанавливать его нельзя"
        }

        if (!pm.canRequestPackageInstalls()) {
            context.startActivity(
                Intent(
                    Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                    Uri.parse("package:${context.packageName}"),
                ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
            return "разрешите установку из ym-sync и нажмите «Обновить» ещё раз"
        }

        val uri = FileProvider.getUriForFile(context, "${context.packageName}.updates", apk)
        context.startActivity(
            Intent(Intent.ACTION_VIEW)
                .setDataAndType(uri, "application/vnd.android.package-archive")
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK),
        )
        return null
    }

    private fun archiveInfo(pm: PackageManager, apk: File): PackageInfo? {
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            PackageManager.GET_SIGNING_CERTIFICATES
        } else {
            @Suppress("DEPRECATION")
            PackageManager.GET_SIGNATURES
        }
        return pm.getPackageArchiveInfo(apk.absolutePath, flags)
    }

    /** Whether the downloaded APK is signed by the same key as what is installed. */
    private fun sameSigner(pm: PackageManager, packageName: String, archive: PackageInfo): Boolean {
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            val installed = pm.getPackageInfo(packageName, PackageManager.GET_SIGNING_CERTIFICATES)
                .signingInfo?.apkContentsSigners ?: return false
            val candidate = archive.signingInfo?.apkContentsSigners ?: return false
            installed.toSet() == candidate.toSet()
        } else {
            @Suppress("DEPRECATION")
            val installed = pm.getPackageInfo(packageName, PackageManager.GET_SIGNATURES).signatures
                ?: return false
            @Suppress("DEPRECATION")
            val candidate = archive.signatures ?: return false
            installed.toSet() == candidate.toSet()
        }
    }
}
