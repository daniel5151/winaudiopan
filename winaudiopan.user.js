// ==UserScript==
// @name         WinAudioPan In-Tab Spatial Panner
// @namespace    https://github.com/daniel5151/winaudiopan
// @version      0.2.2
// @description  Independently pans audio per-tab based on window screen position, active only while the WinAudioPan desktop server is running.
// @match        *://*.youtube.com/*
// @match        *://*.twitch.tv/*
// @match        *://*.soundcloud.com/*
// @match        *://*.spotify.com/*
// @match        *://*.bandcamp.com/*
// @match        *://*.vimeo.com/*
// @grant        GM_xmlhttpRequest
// @connect      127.0.0.1
// @connect      localhost
// @run-at       document-start
// ==/UserScript==

(function () {
    'use strict';

    // Generate a unique identifier for this tab instance so multiple tabs don't cancel each other out
    const TAB_ID = Math.random().toString(36).slice(2, 10);

    const DESKTOP_APP_PORT = 38400;
    const BASE_URL = `http://127.0.0.1:${DESKTOP_APP_PORT}`;
    const CLAIM_URL = `${BASE_URL}/claim?app=chrome&tab=${TAB_ID}`;
    const RELEASE_URL = `${BASE_URL}/release?app=chrome&tab=${TAB_ID}`;
    const PING_URL = `${BASE_URL}/ping`;
    const PAN_STRENGTH = 1.0;

    let audioCtx = null;
    let panner = null;
    let isServerOnline = false;
    let wasPlayingMedia = false;
    let probeTimer = null;
    const hookedMediaElements = new WeakSet();

    /**
     * Initializes the Web Audio context and stereo panner node if not already created.
     */
    function ensureAudioContext() {
        if (!audioCtx) {
            const AudioContextClass = window.AudioContext || window.webkitAudioContext;
            audioCtx = new AudioContextClass();
            panner = audioCtx.createStereoPanner();
            panner.connect(audioCtx.destination);
        }
        if (audioCtx.state === 'suspended') {
            audioCtx.resume().catch(() => {});
        }
    }

    /**
     * Resets stereo panning back to center (0.0).
     */
    function resetPanToCenter() {
        if (panner && audioCtx) {
            panner.pan.setTargetAtTime(0.0, audioCtx.currentTime, 0.05);
        }
    }

    /**
     * Checks if any media elements on this page are currently playing audio.
     */
    function isAnyMediaPlaying() {
        return Array.from(document.querySelectorAll('video, audio'))
            .some(m => !m.paused && !m.ended && m.readyState > 1);
    }

    /**
     * Sends an HTTP request via GM_xmlhttpRequest.
     */
    function sendRequest(url, onSuccess, onError) {
        if (typeof GM_xmlhttpRequest === 'undefined') {
            return;
        }
        GM_xmlhttpRequest({
            method: 'GET',
            url: url,
            timeout: 1500,
            onload: (res) => {
                if (res.status === 200 || res.status === 204) {
                    if (onSuccess) onSuccess();
                } else {
                    if (onError) onError('HTTP ' + res.status);
                }
            },
            onerror: () => { if (onError) onError('connection error'); },
            ontimeout: () => { if (onError) onError('timeout'); },
        });
    }

    /**
     * Sends a one-time release claim to the desktop server when this tab pauses or unloads.
     */
    function sendRelease() {
        sendRequest(RELEASE_URL);
    }

    /**
     * Probes the desktop server. If playing, sends a tab-specific claim heartbeat;
     * if idle, only sends a passive ping so we don't interfere with other active tabs.
     */
    function probeServer() {
        const isPlaying = isAnyMediaPlaying();
        const url = isPlaying ? CLAIM_URL : PING_URL;

        sendRequest(
            url,
            () => {
                if (!isServerOnline) {
                    isServerOnline = true;
                    console.log(`[WinAudioPan] Desktop server online (tab ${TAB_ID}). In-tab spatial panning ENABLED.`);
                }
                if (isPlaying) {
                    scanAndHookMedia();
                }
            },
            (reason) => {
                if (isServerOnline) {
                    isServerOnline = false;
                    console.log(`[WinAudioPan] Desktop server offline (${reason}). In-tab spatial panning DISABLED.`);
                    resetPanToCenter();
                }
            }
        );

        // Check if playback state transitioned from playing to stopped
        if (wasPlayingMedia && !isPlaying) {
            sendRelease();
        }
        wasPlayingMedia = isPlaying;
    }

    /**
     * Hooks an HTML5 media element into the Web Audio panner.
     */
    function hookMediaElement(media) {
        if (!media || hookedMediaElements.has(media) || !isServerOnline) {
            return;
        }

        ensureAudioContext();
        try {
            const source = audioCtx.createMediaElementSource(media);
            source.connect(panner);
            hookedMediaElements.add(media);
            console.log(`[WinAudioPan] Hooked media element on tab ${TAB_ID}.`);
        } catch (err) {
            console.debug('[WinAudioPan] Could not hook media element:', err);
        }
    }

    /**
     * Scans DOM for all video/audio tags and hooks them if the server is online.
     */
    function scanAndHookMedia() {
        if (!isServerOnline) {
            return;
        }
        const mediaElements = document.querySelectorAll('video, audio');
        for (const media of mediaElements) {
            hookMediaElement(media);
        }
    }

    /**
     * Periodically updates the stereo panner based on current window coordinates.
     * Completely no-ops (centers audio) if the desktop server is not online.
     */
    function updatePanPosition() {
        if (!panner || !audioCtx) {
            return;
        }

        // NO-OP when desktop server is not running
        if (!isServerOnline) {
            resetPanToCenter();
            return;
        }

        // If window is minimized or tab hidden, center audio
        if (document.hidden) {
            resetPanToCenter();
            return;
        }

        // Window geometry
        const winWidth = Math.max(0, window.outerWidth || 800);
        const winMidX = window.screenX + (winWidth / 2);

        // Monitor geometry
        const screenLeft = window.screen.availLeft || 0;
        const screenWidth = Math.max(1, window.screen.availWidth || window.screen.width || 1920);

        const normW = Math.max(0, winWidth / screenWidth);
        let pan = 0.0;

        // If window fills 96%+ of the monitor (or is stretched wider), treat as centered
        if (normW < 0.96) {
            const normC = (winMidX - screenLeft) / screenWidth;
            const availableTravel = 1.0 - normW;
            const rawPan = (2.0 * (normC - 0.5)) / availableTravel;
            // Clamping cleanly handles windows extending past the left or right edges of the screen
            const clampedPan = Math.max(-1.0, Math.min(1.0, rawPan));
            let finalPan = clampedPan;
            // Snap threshold so windows touching or within 3% of the bezel hit 100% hard isolation
            if (finalPan >= 0.97) {
                finalPan = 1.0;
            } else if (finalPan <= -0.97) {
                finalPan = -1.0;
            }
            pan = finalPan * PAN_STRENGTH;
        }

        panner.pan.setTargetAtTime(pan, audioCtx.currentTime, 0.05);
    }

    // Capture playback events
    document.addEventListener('play', (event) => {
        if (event.target && (event.target.tagName === 'VIDEO' || event.target.tagName === 'AUDIO')) {
            probeServer();
            if (isServerOnline) {
                hookMediaElement(event.target);
            }
            if (audioCtx && audioCtx.state === 'suspended') {
                audioCtx.resume().catch(() => {});
            }
        }
    }, true);

    document.addEventListener('pause', () => {
        probeServer();
    }, true);

    document.addEventListener('ended', () => {
        probeServer();
    }, true);

    // Release this tab's claim when closing or navigating
    window.addEventListener('beforeunload', () => {
        if (isServerOnline && wasPlayingMedia) {
            sendRelease();
        }
    });

    // Probe the desktop server every 2 seconds
    probeTimer = setInterval(probeServer, 2000);

    // Smooth panning update loop (60ms)
    setInterval(updatePanPosition, 60);

    // Periodic check to hook newly spawned elements
    setInterval(scanAndHookMedia, 2000);

    // Initial probe on script load
    probeServer();
})();
