package dev.mshiv.ymsync

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.util.LruCache
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withContext
import java.net.HttpURLConnection
import java.net.URL

/** A cover, and the colour the full-screen player paints its background with. */
class Cover(val bitmap: Bitmap, val tint: Color) {
    val image: ImageBitmap = bitmap.asImageBitmap()
}

/**
 * Album covers off Yandex's image CDN, kept in memory.
 *
 * Deliberately small: covers are decoration, so there is no disk cache and no
 * image library. With no internet every load fails, and the row keeps its
 * placeholder; a failed address is not retried for a minute so a list scrolling
 * past offline does not hammer a dead connection.
 */
object Covers {
    private val memory = LruCache<String, Cover>(96)
    private val failedAt = HashMap<String, Long>()
    private val gate = Semaphore(4)

    /** What is already in memory, so a recomposed row draws its cover at once. */
    fun peek(url: String?): Cover? = url?.let { memory.get(it) }

    suspend fun load(url: String): Cover? {
        memory.get(url)?.let { return it }
        synchronized(failedAt) {
            val failed = failedAt[url]
            if (failed != null && System.currentTimeMillis() - failed < RETRY_MS) return null
        }
        return gate.withPermit {
            memory.get(url) ?: withContext(Dispatchers.IO) { fetch(url) }?.also { memory.put(url, it) }
        }
    }

    private fun fetch(url: String): Cover? =
        try {
            val connection = URL(url).openConnection() as HttpURLConnection
            connection.connectTimeout = 8_000
            connection.readTimeout = 8_000
            try {
                connection.inputStream.use { BitmapFactory.decodeStream(it) }
            } finally {
                connection.disconnect()
            }?.let { Cover(it, tintOf(it)) }
        } catch (_: Exception) {
            null
        }.also { cover ->
            if (cover == null) synchronized(failedAt) { failedAt[url] = System.currentTimeMillis() }
        }

    /**
     * The cover's average colour, pulled toward a mid brightness: a black cover
     * would give a black player and a white one would wash the text out.
     */
    private fun tintOf(bitmap: Bitmap): Color {
        val pixel = Bitmap.createScaledBitmap(bitmap, 1, 1, true).getPixel(0, 0)
        val hsv = FloatArray(3)
        android.graphics.Color.colorToHSV(pixel, hsv)
        hsv[1] = (hsv[1] * 1.2f).coerceIn(0.25f, 0.85f)
        hsv[2] = hsv[2].coerceIn(0.35f, 0.6f)
        return Color(android.graphics.Color.HSVToColor(hsv))
    }

    private const val RETRY_MS = 60_000L
}
