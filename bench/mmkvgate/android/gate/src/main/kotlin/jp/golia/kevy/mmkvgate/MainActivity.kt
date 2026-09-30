// Runs every axis once on launch, off the main thread, then shows the
// lines. The done marker appears only after the last axis, so a reader
// waiting for it never reads a partial table.
package jp.golia.kevy.mmkvgate

import android.app.Activity
import android.os.Bundle
import android.view.View
import android.widget.LinearLayout
import android.widget.TextView
import com.tencent.mmkv.MMKV

class MainActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val result = TextView(this).apply { id = R.id.mmkvgate_result; text = "running" }
        val done = TextView(this).apply {
            id = R.id.mmkvgate_done
            text = "done"
            visibility = View.GONE
        }
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(done)
            addView(result)
        })
        val root = tmpDir(cacheDir, "run")
        MMKV.initialize(this, tmpDir(root, "mmkv-root").path)
        Thread {
            val lines = mutableListOf<String>()
            Axes(root, lines).all()
            root.deleteRecursively()
            runOnUiThread {
                result.text = lines.joinToString("\n")
                done.visibility = View.VISIBLE
            }
        }.start()
    }
}
