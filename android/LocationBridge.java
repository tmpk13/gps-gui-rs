package rs.gps.gui;

import android.app.Activity;
import android.content.Context;
import android.location.Location;
import android.location.LocationListener;
import android.location.LocationManager;
import android.os.Bundle;
import android.os.Looper;

/**
 * Active GPS location updates driven into the Rust app over JNI.
 *
 * Like BleBridge, this is compiled offline to a dex (android/build-dex.sh),
 * embedded in the native library, and loaded at runtime with DexClassLoader,
 * so it must only reference framework classes. The nativeOnLocation method is
 * implemented in Rust and bound via RegisterNatives.
 *
 * The point of this shim is requestLocationUpdates: unlike
 * getLastKnownLocation, it actually powers up the GNSS hardware and delivers
 * fresh fixes as the device moves. Callbacks are delivered on the main
 * Looper (passed explicitly), so start() can be called from any thread.
 */
public final class LocationBridge {
    private static LocationManager manager;
    private static LocationListener listener;

    private LocationBridge() {}

    private static native void nativeOnLocation(
            double lat, double lon, float bearing, boolean hasBearing);

    /**
     * Register for updates from the first available provider (GPS, else
     * network). Returns false if no provider could be registered. minTimeMs is
     * the minimum interval between callbacks; minDistanceM the minimum movement.
     */
    public static boolean start(Activity activity, long minTimeMs, float minDistanceM) {
        try {
            Context ctx = activity.getApplicationContext();
            manager = (LocationManager) ctx.getSystemService(Context.LOCATION_SERVICE);
            if (manager == null) {
                return false;
            }
            stop();
            listener = new LocationListener() {
                @Override
                public void onLocationChanged(Location loc) {
                    boolean hasBearing = loc.hasBearing();
                    nativeOnLocation(
                            loc.getLatitude(),
                            loc.getLongitude(),
                            hasBearing ? loc.getBearing() : 0f,
                            hasBearing);
                }

                // Abstract on API < 30; override so the class is instantiable
                // there. Harmless no-ops on newer devices.
                @Override
                public void onStatusChanged(String provider, int status, Bundle extras) {}

                @Override
                public void onProviderEnabled(String provider) {}

                @Override
                public void onProviderDisabled(String provider) {}
            };
            // First provider that registers wins: using GPS alone (when present)
            // keeps precise fixes from ping-ponging with coarse network ones.
            String[] providers = {
                LocationManager.GPS_PROVIDER,
                LocationManager.NETWORK_PROVIDER,
            };
            for (String provider : providers) {
                try {
                    manager.requestLocationUpdates(
                            provider, minTimeMs, minDistanceM, listener, Looper.getMainLooper());
                    return true;
                } catch (Throwable t) {
                    // Provider missing/disabled on this device; try the next.
                }
            }
            return false;
        } catch (Throwable t) {
            return false;
        }
    }

    public static void stop() {
        try {
            if (manager != null && listener != null) {
                manager.removeUpdates(listener);
            }
        } catch (Throwable t) {
        }
        listener = null;
    }
}
