package io.github.sonorahq.sonora;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.IntentFilter;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.media.AudioAttributes;
import android.media.AudioFocusRequest;
import android.media.AudioManager;
import android.media.MediaMetadata;
import android.media.session.MediaSession;
import android.media.session.PlaybackState;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.PowerManager;
import android.util.Log;
import android.view.KeyEvent;

public class PlaybackService extends Service {
    private static final String TAG = "SonoraMedia";
    public static final String CHANNEL_ID = "sonora_playback";
    public static final int NOTIFICATION_ID = 1001;
    public static final String ACTION_PLAY_PAUSE = "io.github.sonorahq.sonora.ACTION_PLAY_PAUSE";
    public static final String ACTION_NEXT = "io.github.sonorahq.sonora.ACTION_NEXT";
    public static final String ACTION_PREVIOUS = "io.github.sonorahq.sonora.ACTION_PREVIOUS";
    public static final String ACTION_STOP = "io.github.sonorahq.sonora.ACTION_STOP";
    public static final String ACTION_DISMISS = "io.github.sonorahq.sonora.ACTION_DISMISS";
    /** How long nothing may be loaded before the notification goes, so the gap between two
     *  songs does not flash it away. */
    private static final long IDLE_GRACE_MS = 2000L;
    /** How long a paused song keeps its notification before it goes on its own. */
    private static final long PAUSED_KEEP_MS = 10L * 60L * 1000L;

    private static volatile PlaybackService sInstance = null;
    private static volatile String sTitle = "";
    private static volatile String sArtist = "";
    private static volatile String sAlbum = "";
    private static volatile long sDurationMs = 0;
    private static volatile String sCoverPath = "";
    private static volatile Bitmap sCoverBitmap = null;
    private static volatile int sPlaybackState = 0;
    private static volatile long sPositionMs = 0;
    private static volatile float sSpeed = 0f;
    private static final Handler sMainHandler = new Handler(Looper.getMainLooper());

    private MediaSession mMediaSession = null;
    private PowerManager.WakeLock mWakeLock = null;
    private AudioManager mAudioManager = null;
    private AudioFocusRequest mFocusRequest = null;
    private boolean mHasFocus = false;
    private boolean mForeground = false;
    private String mLastMetaKey = "";
    private int mLastNotifState = -1;
    private BroadcastReceiver mNoisyReceiver = null;
    private final Runnable mIdleStop = new Runnable() {
        @Override
        public void run() {
            if (sPlaybackState == 0) {
                removeNotification();
            }
        }
    };
    private final Runnable mPausedStop = new Runnable() {
        @Override
        public void run() {
            if (sPlaybackState == 2) {
                removeNotification();
            }
        }
    };

    public static void syncMetadata(
        Context ctx,
        String title,
        String artist,
        String album,
        long durationMs,
        String coverPath
    ) {
        sTitle = title != null ? title : "";
        sArtist = artist != null ? artist : "";
        sAlbum = album != null ? album : "";
        sDurationMs = Math.max(0L, durationMs);
        String cleanPath = coverPath != null ? coverPath : "";
        if (cleanPath.startsWith("file://")) {
            cleanPath = cleanPath.substring("file://".length());
        }
        if (!cleanPath.equals(sCoverPath)) {
            sCoverPath = cleanPath;
            sCoverBitmap = decodeScaledBitmap(cleanPath, 512);
        }
        syncService(ctx);
    }

    public static void syncPlayback(Context ctx, int stateCode, long positionMs, float speed) {
        sPlaybackState = stateCode;
        sPositionMs = Math.max(0L, positionMs);
        sSpeed = speed;
        syncService(ctx);
    }

    public static void refreshIfActive(Context ctx) {
        if (ctx != null && (!sTitle.isEmpty() || sPlaybackState != 0)) {
            syncService(ctx);
        }
    }

    private static void syncService(Context ctx) {
        if (ctx == null) {
            return;
        }
        final Context appCtx = ctx.getApplicationContext();
        sMainHandler.post(new Runnable() {
            @Override
            public void run() {
                PlaybackService svc = sInstance;
                if (svc != null) {
                    svc.updateNotificationAndLocks();
                    return;
                }
                if (sPlaybackState != 0) {
                    try {
                        Intent intent = new Intent(appCtx, PlaybackService.class);
                        if (Build.VERSION.SDK_INT >= 26) {
                            appCtx.startForegroundService(intent);
                        } else {
                            appCtx.startService(intent);
                        }
                    } catch (Throwable t) {
                        SonoraActivity.logMedia("cannot start PlaybackService: " + t);
                        Log.w(TAG, "cannot start PlaybackService: " + t);
                    }
                }
            }
        });
    }

    private static Bitmap decodeScaledBitmap(String path, int maxDim) {
        if (path == null || path.isEmpty()) {
            return null;
        }
        try {
            BitmapFactory.Options bounds = new BitmapFactory.Options();
            bounds.inJustDecodeBounds = true;
            BitmapFactory.decodeFile(path, bounds);
            int sample = 1;
            int w = bounds.outWidth;
            int h = bounds.outHeight;
            while (w / sample > maxDim || h / sample > maxDim) {
                sample *= 2;
            }
            BitmapFactory.Options opts = new BitmapFactory.Options();
            opts.inSampleSize = Math.max(1, sample);
            return BitmapFactory.decodeFile(path, opts);
        } catch (Throwable t) {
            return null;
        }
    }

    @Override
    public void onCreate() {
        super.onCreate();
        sInstance = this;
        try {
            NotificationManager nm = (NotificationManager) getSystemService(Context.NOTIFICATION_SERVICE);
            if (nm != null && Build.VERSION.SDK_INT >= 26) {
                NotificationChannel channel = new NotificationChannel(
                    CHANNEL_ID,
                    "Playback",
                    NotificationManager.IMPORTANCE_LOW
                );
                channel.setDescription("Current playing track and playback controls");
                channel.setShowBadge(false);
                channel.setLockscreenVisibility(Notification.VISIBILITY_PUBLIC);
                nm.createNotificationChannel(channel);
                SonoraActivity.logMedia("PlaybackService created, notificationsEnabled=" + nm.areNotificationsEnabled());
            }

            PowerManager pm = (PowerManager) getSystemService(Context.POWER_SERVICE);
            if (pm != null) {
                mWakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "Sonora:Playback");
                mWakeLock.setReferenceCounted(false);
            }
            mAudioManager = (AudioManager) getSystemService(Context.AUDIO_SERVICE);

            // Headphones unplugged or a Bluetooth headset gone: pause rather than carry on
            // out of the phone's speaker.
            mNoisyReceiver = new BroadcastReceiver() {
                @Override
                public void onReceive(Context context, Intent intent) {
                    if (intent != null
                            && AudioManager.ACTION_AUDIO_BECOMING_NOISY.equals(intent.getAction())
                            && (sPlaybackState == 1 || sPlaybackState == 3)) {
                        SonoraActivity.logMedia("audio output went away, pausing");
                        SonoraActivity.sendMediaCommand("PAUSE");
                    }
                }
            };
            IntentFilter noisy = new IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY);
            if (Build.VERSION.SDK_INT >= 33) {
                registerReceiver(mNoisyReceiver, noisy, Context.RECEIVER_NOT_EXPORTED);
            } else {
                registerReceiver(mNoisyReceiver, noisy);
            }

            mMediaSession = new MediaSession(this, "Sonora");
            mMediaSession.setFlags(
                MediaSession.FLAG_HANDLES_MEDIA_BUTTONS
                    | MediaSession.FLAG_HANDLES_TRANSPORT_CONTROLS
            );
            mMediaSession.setCallback(new MediaSession.Callback() {
                @Override
                public boolean onMediaButtonEvent(Intent mediaButtonIntent) {
                    if (mediaButtonIntent != null && Intent.ACTION_MEDIA_BUTTON.equals(mediaButtonIntent.getAction())) {
                        KeyEvent event = mediaButtonIntent.getParcelableExtra(Intent.EXTRA_KEY_EVENT);
                        if (event != null) {
                            int code = event.getKeyCode();
                            if (isHandledMediaKey(code)) {
                                if (event.getAction() == KeyEvent.ACTION_DOWN && event.getRepeatCount() == 0) {
                                    SonoraActivity.handleMediaKeyCode(code);
                                }
                                return true;
                            }
                        }
                    }
                    return super.onMediaButtonEvent(mediaButtonIntent);
                }

                @Override
                public void onPlay() {
                    SonoraActivity.sendMediaCommand("PLAY");
                }

                @Override
                public void onPause() {
                    SonoraActivity.sendMediaCommand("PAUSE");
                }

                @Override
                public void onSkipToNext() {
                    SonoraActivity.sendMediaCommand("NEXT");
                }

                @Override
                public void onSkipToPrevious() {
                    SonoraActivity.sendMediaCommand("PREVIOUS");
                }

                @Override
                public void onSeekTo(long pos) {
                    SonoraActivity.sendMediaCommand("SEEK:" + Math.max(0L, pos));
                }

                @Override
                public void onStop() {
                    SonoraActivity.sendMediaCommand("PAUSE");
                }
            });
        } catch (Throwable t) {
            SonoraActivity.logMedia("PlaybackService onCreate failed: " + t);
            Log.e(TAG, "onCreate failed: " + t);
        }
    }

    private static boolean isHandledMediaKey(int code) {
        return code == KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE
            || code == KeyEvent.KEYCODE_HEADSETHOOK
            || code == KeyEvent.KEYCODE_MEDIA_PLAY
            || code == KeyEvent.KEYCODE_MEDIA_PAUSE
            || code == KeyEvent.KEYCODE_MEDIA_STOP
            || code == KeyEvent.KEYCODE_MEDIA_NEXT
            || code == KeyEvent.KEYCODE_MEDIA_PREVIOUS;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && intent.getAction() != null) {
            String action = intent.getAction();
            if (ACTION_PLAY_PAUSE.equals(action)) {
                SonoraActivity.sendMediaCommand("TOGGLE");
            } else if (ACTION_NEXT.equals(action)) {
                SonoraActivity.sendMediaCommand("NEXT");
            } else if (ACTION_PREVIOUS.equals(action)) {
                SonoraActivity.sendMediaCommand("PREVIOUS");
            } else if (ACTION_STOP.equals(action)) {
                SonoraActivity.sendMediaCommand("PAUSE");
            } else if (ACTION_DISMISS.equals(action)) {
                // swiped away while paused
                if (sPlaybackState != 1 && sPlaybackState != 3) {
                    removeNotification();
                }
                return START_NOT_STICKY;
            }
        }

        if (!mForeground) {
            try {
                Notification initial = buildNotification();
                if (Build.VERSION.SDK_INT >= 29) {
                    startForeground(
                        NOTIFICATION_ID,
                        initial,
                        ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
                    );
                } else {
                    startForeground(NOTIFICATION_ID, initial);
                }
                mForeground = true;
                SonoraActivity.logMedia("PlaybackService foreground started");
            } catch (Throwable t) {
                SonoraActivity.logMedia("initial startForeground failed: " + t);
            }
        }
        updateNotificationAndLocks();
        return START_NOT_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    private PendingIntent actionIntent(String action, int requestCode) {
        Intent intent = new Intent(this, PlaybackService.class);
        intent.setAction(action);
        int flags = PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE;
        return PendingIntent.getService(this, requestCode, intent, flags);
    }

    private void updateAudioFocus(boolean active) {
        if (mAudioManager == null) {
            return;
        }
        try {
            if (active && !mHasFocus) {
                if (Build.VERSION.SDK_INT >= 26) {
                    if (mFocusRequest == null) {
                        AudioAttributes attrs = new AudioAttributes.Builder()
                            .setUsage(AudioAttributes.USAGE_MEDIA)
                            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                            .build();
                        mFocusRequest = new AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                            .setAudioAttributes(attrs)
                            .setOnAudioFocusChangeListener(new AudioManager.OnAudioFocusChangeListener() {
                                @Override
                                public void onAudioFocusChange(int focusChange) {
                                    if (focusChange == AudioManager.AUDIOFOCUS_LOSS
                                            || focusChange == AudioManager.AUDIOFOCUS_LOSS_TRANSIENT) {
                                        mHasFocus = false;
                                        SonoraActivity.sendMediaCommand("PAUSE");
                                    }
                                }
                            })
                            .build();
                    }
                    int res = mAudioManager.requestAudioFocus(mFocusRequest);
                    mHasFocus = res == AudioManager.AUDIOFOCUS_REQUEST_GRANTED;
                }
            } else if (!active && mHasFocus) {
                if (Build.VERSION.SDK_INT >= 26 && mFocusRequest != null) {
                    mAudioManager.abandonAudioFocusRequest(mFocusRequest);
                }
                mHasFocus = false;
            }
        } catch (Throwable ignored) {
        }
    }

    private void updateWakeLock(boolean active) {
        if (mWakeLock == null) {
            return;
        }
        try {
            if (active && !mWakeLock.isHeld()) {
                mWakeLock.acquire();
            } else if (!active && mWakeLock.isHeld()) {
                mWakeLock.release();
            }
        } catch (Throwable ignored) {
        }
    }

    private Notification buildNotification() {
        Intent launch = new Intent(this, SonoraActivity.class);
        launch.addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP | Intent.FLAG_ACTIVITY_CLEAR_TOP);
        PendingIntent contentIntent = PendingIntent.getActivity(
            this,
            0,
            launch,
            PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE
        );

        boolean playing = sPlaybackState == 1;
        int playIcon = playing ? android.R.drawable.ic_media_pause : android.R.drawable.ic_media_play;
        String playTitle = playing ? "Pause" : "Play";

        Notification.Action prevAction = new Notification.Action.Builder(
            android.R.drawable.ic_media_previous,
            "Previous",
            actionIntent(ACTION_PREVIOUS, 1)
        ).build();
        Notification.Action playAction = new Notification.Action.Builder(
            playIcon,
            playTitle,
            actionIntent(ACTION_PLAY_PAUSE, 2)
        ).build();
        Notification.Action nextAction = new Notification.Action.Builder(
            android.R.drawable.ic_media_next,
            "Next",
            actionIntent(ACTION_NEXT, 3)
        ).build();

        int smallIcon = getResources().getIdentifier("ic_stat_sonora", "drawable", getPackageName());
        if (smallIcon == 0) {
            smallIcon = android.R.drawable.ic_media_play;
        }

        String title = !sTitle.isEmpty() ? sTitle : "Sonora";
        Notification.Builder builder = new Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(smallIcon)
            .setContentTitle(title)
            .setContentText(sArtist)
            .setSubText(sAlbum)
            .setContentIntent(contentIntent)
            .setVisibility(Notification.VISIBILITY_PUBLIC)
            .setOngoing(playing)
            .setOnlyAlertOnce(true)
            .setDeleteIntent(actionIntent(ACTION_DISMISS, 4))
            .addAction(prevAction)
            .addAction(playAction)
            .addAction(nextAction);

        if (sCoverBitmap != null) {
            builder.setLargeIcon(sCoverBitmap);
        }
        if (mMediaSession != null) {
            builder.setStyle(
                new Notification.MediaStyle()
                    .setMediaSession(mMediaSession.getSessionToken())
                    .setShowActionsInCompactView(0, 1, 2)
            );
        }
        return builder.build();
    }

    /** Takes the notification down and ends the service, with nothing left to control. */
    private void removeNotification() {
        try {
            sMainHandler.removeCallbacks(mIdleStop);
            sMainHandler.removeCallbacks(mPausedStop);
            updateWakeLock(false);
            updateAudioFocus(false);
            if (mMediaSession != null) {
                mMediaSession.setActive(false);
                PlaybackState stopped = new PlaybackState.Builder()
                    .setState(PlaybackState.STATE_STOPPED, 0L, 0f)
                    .build();
                mMediaSession.setPlaybackState(stopped);
            }
            mLastNotifState = 0;
            mLastMetaKey = "";
            if (mForeground) {
                stopForeground(STOP_FOREGROUND_REMOVE);
                mForeground = false;
            }
            NotificationManager nm = (NotificationManager) getSystemService(Context.NOTIFICATION_SERVICE);
            if (nm != null) {
                nm.cancel(NOTIFICATION_ID);
            }
        } catch (Throwable t) {
            SonoraActivity.logMedia("removeNotification failed: " + t);
        }
        stopSelf();
    }

    @Override
    public void onTaskRemoved(Intent rootIntent) {
        // swiped out of recents with nothing playing: nothing should stay behind
        if (sPlaybackState != 1 && sPlaybackState != 3) {
            removeNotification();
        }
        super.onTaskRemoved(rootIntent);
    }

    private void updateNotificationAndLocks() {
        try {
            NotificationManager nm = (NotificationManager) getSystemService(Context.NOTIFICATION_SERVICE);
            boolean activeTrack = sPlaybackState != 0;
            boolean playing = sPlaybackState == 1 || sPlaybackState == 3;

            updateWakeLock(playing);
            updateAudioFocus(sPlaybackState == 1);

            if (!activeTrack) {
                sMainHandler.removeCallbacks(mPausedStop);
                sMainHandler.removeCallbacks(mIdleStop);
                sMainHandler.postDelayed(mIdleStop, IDLE_GRACE_MS);
                return;
            }
            sMainHandler.removeCallbacks(mIdleStop);
            if (sPlaybackState == 2) {
                if (mLastNotifState != 2) {
                    sMainHandler.removeCallbacks(mPausedStop);
                    sMainHandler.postDelayed(mPausedStop, PAUSED_KEEP_MS);
                }
            } else {
                sMainHandler.removeCallbacks(mPausedStop);
            }

            String metaKey = sTitle + "\n" + sArtist + "\n" + sAlbum + "\n" + sDurationMs + "\n" + sCoverPath;
            boolean metaChanged = !metaKey.equals(mLastMetaKey);
            boolean stateChanged = sPlaybackState != mLastNotifState;

            if (mMediaSession != null) {
                if (!mMediaSession.isActive()) {
                    mMediaSession.setActive(true);
                }
                if (metaChanged) {
                    MediaMetadata.Builder meta = new MediaMetadata.Builder()
                        .putString(MediaMetadata.METADATA_KEY_TITLE, !sTitle.isEmpty() ? sTitle : "Sonora")
                        .putString(MediaMetadata.METADATA_KEY_ARTIST, sArtist)
                        .putString(MediaMetadata.METADATA_KEY_ALBUM, sAlbum);
                    if (sDurationMs > 0) {
                        meta.putLong(MediaMetadata.METADATA_KEY_DURATION, sDurationMs);
                    }
                    if (sCoverBitmap != null) {
                        meta.putBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART, sCoverBitmap);
                    }
                    mMediaSession.setMetadata(meta.build());
                }

                int state;
                switch (sPlaybackState) {
                    case 1:
                        state = PlaybackState.STATE_PLAYING;
                        break;
                    case 2:
                        state = PlaybackState.STATE_PAUSED;
                        break;
                    case 3:
                        state = PlaybackState.STATE_BUFFERING;
                        break;
                    default:
                        state = PlaybackState.STATE_NONE;
                        break;
                }
                long actions = PlaybackState.ACTION_PLAY
                    | PlaybackState.ACTION_PAUSE
                    | PlaybackState.ACTION_PLAY_PAUSE
                    | PlaybackState.ACTION_SKIP_TO_NEXT
                    | PlaybackState.ACTION_SKIP_TO_PREVIOUS
                    | PlaybackState.ACTION_SEEK_TO
                    | PlaybackState.ACTION_STOP;
                PlaybackState pbState = new PlaybackState.Builder()
                    .setActions(actions)
                    .setState(state, sPositionMs, sSpeed)
                    .build();
                mMediaSession.setPlaybackState(pbState);
            }

            // Playing keeps the service in the foreground so Android lets it run. Paused lets go
            // of the foreground but leaves the notification up, now swipeable, to resume from.
            boolean wantForeground = sPlaybackState == 1 || sPlaybackState == 3;
            if (metaChanged || stateChanged || (wantForeground && !mForeground)) {
                Notification notification = buildNotification();
                boolean shown = false;
                if (wantForeground && !mForeground) {
                    try {
                        if (Build.VERSION.SDK_INT >= 29) {
                            startForeground(
                                NOTIFICATION_ID,
                                notification,
                                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
                            );
                        } else {
                            startForeground(NOTIFICATION_ID, notification);
                        }
                        mForeground = true;
                        shown = true;
                    } catch (Throwable t) {
                        SonoraActivity.logMedia("startForeground refused: " + t);
                    }
                }
                if (!shown && nm != null) {
                    nm.notify(NOTIFICATION_ID, notification);
                }
                if (!wantForeground && mForeground) {
                    stopForeground(STOP_FOREGROUND_DETACH);
                    mForeground = false;
                }
            }
            mLastMetaKey = metaKey;
            mLastNotifState = sPlaybackState;
        } catch (Throwable t) {
            SonoraActivity.logMedia("updateNotificationAndLocks failed: " + t);
            Log.e(TAG, "updateNotificationAndLocks failed: " + t);
        }
    }

    @Override
    public void onDestroy() {
        sMainHandler.removeCallbacks(mIdleStop);
        sMainHandler.removeCallbacks(mPausedStop);
        if (mNoisyReceiver != null) {
            try {
                unregisterReceiver(mNoisyReceiver);
            } catch (Throwable ignored) {
            }
            mNoisyReceiver = null;
        }
        try {
            updateWakeLock(false);
            updateAudioFocus(false);
            if (mMediaSession != null) {
                mMediaSession.setActive(false);
                mMediaSession.release();
                mMediaSession = null;
            }
            if (mForeground) {
                stopForeground(STOP_FOREGROUND_REMOVE);
                mForeground = false;
            }
        } catch (Throwable ignored) {
        }
        if (sInstance == this) {
            sInstance = null;
        }
        super.onDestroy();
    }
}
