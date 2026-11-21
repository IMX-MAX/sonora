package io.github.sonorahq.sonora;

import android.Manifest;
import android.app.NativeActivity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.res.Resources;
import android.graphics.Color;
import android.graphics.Insets;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Looper;
import android.text.InputType;
import android.view.KeyEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;

import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

public class SonoraActivity extends NativeActivity {
    public static volatile int sTop = 0;
    public static volatile int sBottom = 0;
    public static volatile int sLeft = 0;
    public static volatile int sRight = 0;
    public static volatile int sImeBottom = 0;

    private static volatile SonoraActivity sInstance = null;
    private static volatile Context sAppContext = null;
    private static volatile boolean sAskedNotifications = false;
    private static final ConcurrentLinkedQueue<String> sInputQueue = new ConcurrentLinkedQueue<String>();
    private View mImeView = null;

    public static native void nativeMediaCommand(String cmd);
    public static native void nativeMediaLog(String msg);

    public static void sendMediaCommand(String cmd) {
        try {
            nativeMediaCommand(cmd);
        } catch (Throwable t) {
            logMedia("sendMediaCommand failed for " + cmd + ": " + t);
        }
    }

    public static void logMedia(String msg) {
        try {
            nativeMediaLog(msg);
        } catch (Throwable ignored) {
        }
    }

    public static boolean handleMediaKeyCode(int code) {
        if (code == KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE || code == KeyEvent.KEYCODE_HEADSETHOOK) {
            sendMediaCommand("TOGGLE");
            return true;
        }
        if (code == KeyEvent.KEYCODE_MEDIA_PLAY) {
            sendMediaCommand("PLAY");
            return true;
        }
        if (code == KeyEvent.KEYCODE_MEDIA_PAUSE || code == KeyEvent.KEYCODE_MEDIA_STOP) {
            sendMediaCommand("PAUSE");
            return true;
        }
        if (code == KeyEvent.KEYCODE_MEDIA_NEXT) {
            sendMediaCommand("NEXT");
            return true;
        }
        if (code == KeyEvent.KEYCODE_MEDIA_PREVIOUS) {
            sendMediaCommand("PREVIOUS");
            return true;
        }
        return false;
    }

    public static int[] getSafeInsets() {
        return new int[] { sTop, sBottom, sLeft, sRight, sImeBottom };
    }

    public static String[] drainInputEvents() {
        int size = sInputQueue.size();
        if (size == 0) {
            return new String[0];
        }
        java.util.ArrayList<String> list = new java.util.ArrayList<String>(size);
        while (true) {
            String item = sInputQueue.poll();
            if (item == null) {
                break;
            }
            list.add(item);
        }
        return list.toArray(new String[0]);
    }

    public static void showSoftKeyboard() {
        final SonoraActivity act = sInstance;
        if (act == null) {
            return;
        }
        act.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    if (act.mImeView != null) {
                        act.mImeView.setFocusable(true);
                        act.mImeView.setFocusableInTouchMode(true);
                        act.mImeView.requestFocus();
                        InputMethodManager imm = (InputMethodManager) act.getSystemService(Context.INPUT_METHOD_SERVICE);
                        if (imm != null) {
                            imm.showSoftInput(act.mImeView, InputMethodManager.SHOW_IMPLICIT);
                        }
                    }
                } catch (Throwable ignored) {
                }
            }
        });
    }

    /** Answers a press with the system's short haptic tick, which follows the phone's own touch feedback setting. */
    public static void hapticTap() {
        final SonoraActivity act = sInstance;
        if (act == null) {
            return;
        }
        act.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    act.getWindow().getDecorView().performHapticFeedback(
                            android.view.HapticFeedbackConstants.VIRTUAL_KEY);
                } catch (Throwable ignored) {
                }
            }
        });
    }

    /** Keeps the screen awake while `on` is not zero, and lets it sleep again otherwise. */
    public static void keepScreenOn(final int on) {
        final SonoraActivity act = sInstance;
        if (act == null) {
            return;
        }
        act.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    if (on != 0) {
                        act.getWindow().addFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
                    } else {
                        act.getWindow().clearFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
                    }
                } catch (Throwable ignored) {
                }
            }
        });
    }

    /** Sends the app to the background, as the back button does on the first page. */
    public static void leaveApp() {
        final SonoraActivity act = sInstance;
        if (act == null) {
            return;
        }
        act.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    act.moveTaskToBack(true);
                } catch (Throwable ignored) {
                }
            }
        });
    }

    public static void hideSoftKeyboard() {
        final SonoraActivity act = sInstance;
        if (act == null) {
            return;
        }
        act.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    View view = act.mImeView != null ? act.mImeView : (act.getWindow() != null ? act.getWindow().getDecorView() : null);
                    if (view != null) {
                        InputMethodManager imm = (InputMethodManager) act.getSystemService(Context.INPUT_METHOD_SERVICE);
                        if (imm != null) {
                            imm.hideSoftInputFromWindow(view.getWindowToken(), 0);
                        }
                    }
                    act.releaseImeFocus();
                } catch (Throwable ignored) {
                }
            }
        });
    }

    /**
     * Takes focus off the keyboard's stand-in view and gives it back to NativeActivity's own
     * view. Left on the stand-in, or on nothing at all, focus can keep touches from reaching
     * the native input queue once the keyboard has gone, which leaves the app ignoring taps.
     */
    private void releaseImeFocus() {
        if (mImeView != null) {
            mImeView.clearFocus();
            mImeView.setFocusable(false);
            mImeView.setFocusableInTouchMode(false);
        }
        View host = nativeView();
        if (host != null) {
            host.setFocusable(true);
            host.setFocusableInTouchMode(true);
            host.requestFocus();
        }
        sInputQueue.add("I:HIDDEN");
    }

    /** NativeActivity's own content view, the sibling the keyboard's stand-in was added beside. */
    private View nativeView() {
        if (mImeView == null || !(mImeView.getParent() instanceof ViewGroup)) {
            return null;
        }
        ViewGroup parent = (ViewGroup) mImeView.getParent();
        for (int i = 0; i < parent.getChildCount(); i++) {
            View child = parent.getChildAt(i);
            if (child != mImeView) {
                return child;
            }
        }
        return null;
    }

    public static void updateMediaMetadata(
        final String title,
        final String artist,
        final String album,
        final long durationMs,
        final String coverPath
    ) {
        final Context ctx = sAppContext != null ? sAppContext : sInstance;
        if (ctx == null) {
            return;
        }
        PlaybackService.syncMetadata(ctx, title, artist, album, durationMs, coverPath);
    }

    public static void updateMediaPlayback(final int stateCode, final long positionMs, final float speed) {
        final Context ctx = sAppContext != null ? sAppContext : sInstance;
        if (ctx == null) {
            return;
        }
        PlaybackService.syncPlayback(ctx, stateCode, positionMs, speed);
    }

    public static String readClipboard() {
        final SonoraActivity act = sInstance;
        if (act == null) {
            return null;
        }
        if (Looper.myLooper() == Looper.getMainLooper()) {
            return readClipboardOnUi(act);
        }
        final AtomicReference<String> result = new AtomicReference<String>(null);
        final CountDownLatch latch = new CountDownLatch(1);
        act.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    result.set(readClipboardOnUi(act));
                } finally {
                    latch.countDown();
                }
            }
        });
        try {
            latch.await(300, TimeUnit.MILLISECONDS);
        } catch (InterruptedException ignored) {
        }
        return result.get();
    }

    private static String readClipboardOnUi(Context ctx) {
        try {
            ClipboardManager cm = (ClipboardManager) ctx.getSystemService(Context.CLIPBOARD_SERVICE);
            if (cm == null || !cm.hasPrimaryClip()) {
                return null;
            }
            ClipData clip = cm.getPrimaryClip();
            if (clip == null || clip.getItemCount() == 0) {
                return null;
            }
            CharSequence text = clip.getItemAt(0).coerceToText(ctx);
            return text != null ? text.toString() : null;
        } catch (Throwable t) {
            return null;
        }
    }

    public static void writeClipboard(final String text) {
        final SonoraActivity act = sInstance;
        if (act == null || text == null) {
            return;
        }
        act.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    ClipboardManager cm = (ClipboardManager) act.getSystemService(Context.CLIPBOARD_SERVICE);
                    if (cm != null) {
                        cm.setPrimaryClip(ClipData.newPlainText("Sonora", text));
                    }
                } catch (Throwable ignored) {
                }
            }
        });
    }

    public static void openUrl(final String url) {
        final SonoraActivity act = sInstance;
        if (act == null || url == null) {
            return;
        }
        act.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    Intent intent = new Intent(Intent.ACTION_VIEW, Uri.parse(url));
                    intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                    act.startActivity(intent);
                } catch (Throwable ignored) {
                }
            }
        });
    }

    private int dimenPx(String name) {
        try {
            Resources res = getResources();
            int id = res.getIdentifier(name, "dimen", "android");
            return id > 0 ? res.getDimensionPixelSize(id) : 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    private class ImeProxyView extends View {
        public ImeProxyView(Context context) {
            super(context);
            setFocusable(false);
            setFocusableInTouchMode(false);
        }

        @Override
        public boolean onCheckIsTextEditor() {
            return true;
        }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
            outAttrs.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD;
            outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI | EditorInfo.IME_ACTION_DONE;
            return new BaseInputConnection(this, false) {
                /** The word the keyboard is composing, already typed into the field. */
                private String mComposing = "";

                /** Turns the composed word into `next`, typing only what differs. */
                private void retype(String next) {
                    int same = 0;
                    int most = Math.min(mComposing.length(), next.length());
                    while (same < most && mComposing.charAt(same) == next.charAt(same)) {
                        same++;
                    }
                    int erase = mComposing.length() - same;
                    if (erase > 0) {
                        sInputQueue.add("B:" + erase);
                    }
                    if (next.length() > same) {
                        sInputQueue.add("T:" + next.substring(same));
                    }
                    mComposing = next;
                }

                @Override
                public boolean commitText(CharSequence text, int newCursorPosition) {
                    retype(text != null ? text.toString() : "");
                    mComposing = "";
                    return true;
                }

                @Override
                public boolean setComposingText(CharSequence text, int newCursorPosition) {
                    retype(text != null ? text.toString() : "");
                    return true;
                }

                @Override
                public boolean finishComposingText() {
                    mComposing = "";
                    return true;
                }

                @Override
                public boolean deleteSurroundingText(int beforeLength, int afterLength) {
                    if (beforeLength > 0) {
                        sInputQueue.add("B:" + beforeLength);
                    }
                    return true;
                }

                @Override
                public boolean sendKeyEvent(KeyEvent event) {
                    if (handleKey(event)) {
                        return true;
                    }
                    return super.sendKeyEvent(event);
                }

                @Override
                public boolean performEditorAction(int editorAction) {
                    sInputQueue.add("K:ENTER");
                    return true;
                }
            };
        }
    }

    private boolean handleKey(KeyEvent event) {
        if (event == null) {
            return false;
        }
        int code = event.getKeyCode();
        if (event.getAction() == KeyEvent.ACTION_DOWN && handleMediaKeyCode(code)) {
            return true;
        }
        if (code == KeyEvent.KEYCODE_BACK
                || code == KeyEvent.KEYCODE_VOLUME_UP
                || code == KeyEvent.KEYCODE_VOLUME_DOWN
                || code == KeyEvent.KEYCODE_VOLUME_MUTE) {
            return false;
        }
        if (event.getAction() == KeyEvent.ACTION_DOWN) {
            if (code == KeyEvent.KEYCODE_DEL) {
                sInputQueue.add("K:BACKSPACE");
                return true;
            }
            if (code == KeyEvent.KEYCODE_ENTER || code == KeyEvent.KEYCODE_NUMPAD_ENTER) {
                sInputQueue.add("K:ENTER");
                return true;
            }
            if (code == KeyEvent.KEYCODE_TAB) {
                sInputQueue.add("K:TAB");
                return true;
            }
            if (code == KeyEvent.KEYCODE_DPAD_LEFT) {
                sInputQueue.add("K:LEFT");
                return true;
            }
            if (code == KeyEvent.KEYCODE_DPAD_RIGHT) {
                sInputQueue.add("K:RIGHT");
                return true;
            }
            if (code == KeyEvent.KEYCODE_DPAD_UP) {
                sInputQueue.add("K:UP");
                return true;
            }
            if (code == KeyEvent.KEYCODE_DPAD_DOWN) {
                sInputQueue.add("K:DOWN");
                return true;
            }
            int ch = event.getUnicodeChar();
            if (ch > 0 && !event.isSystem()) {
                sInputQueue.add("T:" + new String(Character.toChars(ch)));
                return true;
            }
        } else if (event.getAction() == KeyEvent.ACTION_MULTIPLE) {
            String chars = event.getCharacters();
            if (chars != null && !chars.isEmpty()) {
                sInputQueue.add("T:" + chars);
                return true;
            }
        }
        return false;
    }

    @Override
    public boolean dispatchKeyEvent(KeyEvent event) {
        try {
            if (handleKey(event)) {
                return true;
            }
        } catch (Throwable ignored) {
        }
        return super.dispatchKeyEvent(event);
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions, int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        if (requestCode == 100) {
            PlaybackService.refreshIfActive(this);
        }
    }

    /**
     * Copies a finished export at {@code path} into the shared Music/Sonora folder as
     * {@code name}, and answers where it went, or a message starting with "!" when it could not.
     * Android 10 and later take it through MediaStore, which needs no permission; older versions
     * write the folder directly and ask for storage access the first time. Called from a Rust
     * worker thread; it blocks on the copy.
     */
    public static String saveToMusic(String path, String name, String mime) {
        Context ctx = sAppContext;
        if (ctx == null) {
            return "!Sonora is not ready";
        }
        java.io.File source = new java.io.File(path);
        try {
            if (Build.VERSION.SDK_INT >= 29) {
                android.content.ContentResolver resolver = ctx.getContentResolver();
                android.content.ContentValues values = new android.content.ContentValues();
                values.put(android.provider.MediaStore.MediaColumns.DISPLAY_NAME, name);
                values.put(android.provider.MediaStore.MediaColumns.MIME_TYPE, mime);
                values.put(
                    android.provider.MediaStore.MediaColumns.RELATIVE_PATH,
                    android.os.Environment.DIRECTORY_MUSIC + "/Sonora"
                );
                values.put(android.provider.MediaStore.MediaColumns.IS_PENDING, 1);
                Uri collection = android.provider.MediaStore.Audio.Media.getContentUri(
                    android.provider.MediaStore.VOLUME_EXTERNAL_PRIMARY
                );
                Uri item = resolver.insert(collection, values);
                if (item == null) {
                    return "!Android would not create the file";
                }
                try (java.io.InputStream in = new java.io.FileInputStream(source);
                     java.io.OutputStream out = resolver.openOutputStream(item)) {
                    if (out == null) {
                        throw new java.io.IOException("cannot open the new file");
                    }
                    copyStream(in, out);
                } catch (Throwable t) {
                    resolver.delete(item, null, null);
                    throw t;
                }
                values.clear();
                values.put(android.provider.MediaStore.MediaColumns.IS_PENDING, 0);
                resolver.update(item, values, null, null);
                String shown = name;
                try (android.database.Cursor cursor = resolver.query(
                        item,
                        new String[] { android.provider.MediaStore.MediaColumns.DISPLAY_NAME },
                        null, null, null)) {
                    if (cursor != null && cursor.moveToFirst() && cursor.getString(0) != null) {
                        shown = cursor.getString(0);
                    }
                } catch (Throwable ignored) {
                }
                return "Music/Sonora/" + shown;
            }
            if (ctx.checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE)
                    != PackageManager.PERMISSION_GRANTED) {
                final SonoraActivity act = sInstance;
                if (act != null) {
                    act.runOnUiThread(new Runnable() {
                        @Override
                        public void run() {
                            act.requestPermissions(
                                new String[] { Manifest.permission.WRITE_EXTERNAL_STORAGE }, 101
                            );
                        }
                    });
                }
                return "!Allow storage access, then save again";
            }
            java.io.File folder = new java.io.File(
                android.os.Environment.getExternalStoragePublicDirectory(
                    android.os.Environment.DIRECTORY_MUSIC
                ),
                "Sonora"
            );
            folder.mkdirs();
            java.io.File target = new java.io.File(folder, name);
            int dot = name.lastIndexOf('.');
            String stem = dot > 0 ? name.substring(0, dot) : name;
            String extension = dot > 0 ? name.substring(dot) : "";
            for (int n = 2; target.exists(); n++) {
                target = new java.io.File(folder, stem + " (" + n + ")" + extension);
            }
            try (java.io.InputStream in = new java.io.FileInputStream(source);
                 java.io.OutputStream out = new java.io.FileOutputStream(target)) {
                copyStream(in, out);
            }
            android.media.MediaScannerConnection.scanFile(
                ctx, new String[] { target.getPath() }, new String[] { mime }, null
            );
            return target.getPath();
        } catch (Throwable t) {
            String message = t.getMessage();
            return "!" + (message != null ? message : t.getClass().getSimpleName());
        }
    }

    private static void copyStream(java.io.InputStream in, java.io.OutputStream out)
            throws java.io.IOException {
        byte[] buffer = new byte[64 * 1024];
        int read;
        while ((read = in.read(buffer)) != -1) {
            out.write(buffer, 0, read);
        }
        out.flush();
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (Build.VERSION.SDK_INT >= 33 && !sAskedNotifications) {
            try {
                if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
                    sAskedNotifications = true;
                    requestPermissions(new String[] { Manifest.permission.POST_NOTIFICATIONS }, 100);
                    return;
                }
            } catch (Throwable ignored) {
            }
        }
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        sInstance = this;
        sAppContext = getApplicationContext();
        sInputQueue.clear();
        if (sTop == 0) {
            sTop = dimenPx("status_bar_height");
        }
        if (sBottom == 0) {
            sBottom = dimenPx("navigation_bar_height");
        }

        super.onCreate(savedInstanceState);

        try {
            mImeView = new ImeProxyView(this);
            addContentView(mImeView, new ViewGroup.LayoutParams(1, 1));
        } catch (Throwable ignored) {
        }

        try {
            Window window = getWindow();
            if (window != null) {
                window.setSoftInputMode(
                    WindowManager.LayoutParams.SOFT_INPUT_STATE_ALWAYS_HIDDEN
                        | WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE
                );
                window.addFlags(WindowManager.LayoutParams.FLAG_DRAWS_SYSTEM_BAR_BACKGROUNDS);
                window.setStatusBarColor(Color.TRANSPARENT);
                window.setNavigationBarColor(Color.TRANSPARENT);
                // The app draws behind the status bar and the navigation or gesture bar, and
                // keeps its own controls clear of them from the insets reported below.
                if (Build.VERSION.SDK_INT >= 30) {
                    window.setDecorFitsSystemWindows(false);
                } else {
                    window.getDecorView().setSystemUiVisibility(
                        View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                    );
                }
                if (Build.VERSION.SDK_INT >= 29) {
                    window.setNavigationBarContrastEnforced(false);
                    window.setStatusBarContrastEnforced(false);
                }
                if (Build.VERSION.SDK_INT >= 28) {
                    WindowManager.LayoutParams attrs = window.getAttributes();
                    attrs.layoutInDisplayCutoutMode =
                        WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
                    window.setAttributes(attrs);
                }
                if (Build.VERSION.SDK_INT >= 30) {
                    WindowInsetsController controller = window.getInsetsController();
                    if (controller != null) {
                        controller.setSystemBarsAppearance(
                            0,
                            WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS
                                | WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS
                        );
                    }
                }
                View decor = window.getDecorView();
                if (decor != null) {
                    decor.setOnApplyWindowInsetsListener(new View.OnApplyWindowInsetsListener() {
                        @Override
                        public WindowInsets onApplyWindowInsets(View v, WindowInsets insets) {
                            try {
                                if (Build.VERSION.SDK_INT >= 30) {
                                    Insets safe = insets.getInsets(
                                        WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout()
                                    );
                                    Insets ime = insets.getInsets(WindowInsets.Type.ime());
                                    sTop = safe.top;
                                    sBottom = safe.bottom;
                                    sLeft = safe.left;
                                    sRight = safe.right;
                                    boolean wasShown = sImeBottom > 0;
                                    sImeBottom = ime.bottom;
                                    if (wasShown && ime.bottom == 0) {
                                        // the keyboard went away, perhaps by the user's back
                                        // gesture rather than the app
                                        releaseImeFocus();
                                    }
                                } else {
                                    sTop = insets.getSystemWindowInsetTop();
                                    sBottom = insets.getSystemWindowInsetBottom();
                                    sLeft = insets.getSystemWindowInsetLeft();
                                    sRight = insets.getSystemWindowInsetRight();
                                    sImeBottom = 0;
                                }
                            } catch (Throwable ignored) {
                            }
                            return v.onApplyWindowInsets(insets);
                        }
                    });
                    decor.requestApplyInsets();
                }
            }
        } catch (Throwable ignored) {
        }
    }

    @Override
    protected void onDestroy() {
        sInputQueue.clear();
        if (sInstance == this) {
            sInstance = null;
        }
        super.onDestroy();
    }
}
