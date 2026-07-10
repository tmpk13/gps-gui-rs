package rs.gps.gui;

import android.app.Activity;
import android.bluetooth.BluetoothAdapter;
import android.bluetooth.BluetoothDevice;
import android.bluetooth.BluetoothGatt;
import android.bluetooth.BluetoothGattCallback;
import android.bluetooth.BluetoothGattCharacteristic;
import android.bluetooth.BluetoothGattDescriptor;
import android.bluetooth.BluetoothGattService;
import android.bluetooth.BluetoothManager;
import android.bluetooth.le.BluetoothLeScanner;
import android.bluetooth.le.ScanCallback;
import android.bluetooth.le.ScanFilter;
import android.bluetooth.le.ScanResult;
import android.bluetooth.le.ScanSettings;
import android.content.Context;
import android.os.Build;
import android.os.ParcelUuid;
import android.view.WindowManager;
import java.util.ArrayList;
import java.util.List;
import java.util.UUID;

/**
 * Minimal BLE central driven by the Rust app over JNI.
 *
 * The app is a NativeActivity packaged by xbuild (no gradle), so this class
 * is compiled offline to a dex (android/build-dex.sh), embedded in the
 * native library, and loaded at runtime with DexClassLoader. It must only
 * reference framework classes. The native* methods are implemented in Rust
 * and registered via RegisterNatives; they are called on Binder threads.
 */
public final class BleBridge {
    private static final UUID CCCD =
            UUID.fromString("00002902-0000-1000-8000-00805f9b34fb");

    private static Context context;
    private static BluetoothAdapter adapter;
    private static BluetoothGatt gatt;
    private static ScanCallback scanCallback;

    private BleBridge() {}

    private static native void nativeOnScan(String address, String name, int rssi);
    private static native void nativeOnConnectionState(int status, int newState);
    private static native void nativeOnServicesDiscovered(int status);
    private static native void nativeOnNotify(String uuid, byte[] value);
    private static native void nativeOnWrite(String uuid, int status);
    private static native void nativeOnDescriptorWrite(int status);

    public static boolean init(Context ctx) {
        try {
            context = ctx.getApplicationContext();
            BluetoothManager m =
                    (BluetoothManager) context.getSystemService(Context.BLUETOOTH_SERVICE);
            adapter = m == null ? null : m.getAdapter();
            return adapter != null;
        } catch (Throwable t) {
            return false;
        }
    }

    /** Scan filtered to the given 128-bit service UUID. */
    public static boolean startScan(String serviceUuid) {
        try {
            BluetoothLeScanner scanner = adapter.getBluetoothLeScanner();
            if (scanner == null) {
                return false; // Bluetooth is turned off.
            }
            stopScan();
            scanCallback = new ScanCallback() {
                @Override
                public void onScanResult(int callbackType, ScanResult result) {
                    BluetoothDevice d = result.getDevice();
                    String name = null;
                    try {
                        name = result.getScanRecord() == null
                                ? null : result.getScanRecord().getDeviceName();
                    } catch (Throwable t) {
                    }
                    nativeOnScan(d.getAddress(), name == null ? "" : name, result.getRssi());
                }
            };
            List<ScanFilter> filters = new ArrayList<ScanFilter>();
            filters.add(new ScanFilter.Builder()
                    .setServiceUuid(new ParcelUuid(UUID.fromString(serviceUuid)))
                    .build());
            ScanSettings settings = new ScanSettings.Builder()
                    .setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY)
                    .build();
            scanner.startScan(filters, settings, scanCallback);
            return true;
        } catch (Throwable t) {
            return false;
        }
    }

    public static void stopScan() {
        try {
            if (scanCallback != null && adapter != null) {
                BluetoothLeScanner scanner = adapter.getBluetoothLeScanner();
                if (scanner != null) {
                    scanner.stopScan(scanCallback);
                }
            }
        } catch (Throwable t) {
        }
        scanCallback = null;
    }

    public static boolean connect(String mac) {
        try {
            disconnect();
            BluetoothDevice device = adapter.getRemoteDevice(mac);
            BluetoothGattCallback cb = new BluetoothGattCallback() {
                @Override
                public void onConnectionStateChange(BluetoothGatt g, int status, int newState) {
                    nativeOnConnectionState(status, newState);
                }

                @Override
                public void onServicesDiscovered(BluetoothGatt g, int status) {
                    nativeOnServicesDiscovered(status);
                }

                // Fires on Android 12 and below (and again on 13+, where it
                // is gated off to avoid double delivery).
                @Override
                public void onCharacteristicChanged(BluetoothGatt g,
                        BluetoothGattCharacteristic ch) {
                    if (Build.VERSION.SDK_INT < 33) {
                        byte[] v = ch.getValue();
                        nativeOnNotify(ch.getUuid().toString(), v == null ? new byte[0] : v);
                    }
                }

                // Fires on Android 13+.
                @Override
                public void onCharacteristicChanged(BluetoothGatt g,
                        BluetoothGattCharacteristic ch, byte[] value) {
                    nativeOnNotify(ch.getUuid().toString(), value);
                }

                @Override
                public void onCharacteristicWrite(BluetoothGatt g,
                        BluetoothGattCharacteristic ch, int status) {
                    nativeOnWrite(ch.getUuid().toString(), status);
                }

                @Override
                public void onDescriptorWrite(BluetoothGatt g, BluetoothGattDescriptor d,
                        int status) {
                    nativeOnDescriptorWrite(status);
                }
            };
            gatt = device.connectGatt(context, false, cb, BluetoothDevice.TRANSPORT_LE);
            return gatt != null;
        } catch (Throwable t) {
            return false;
        }
    }

    public static boolean discoverServices() {
        try {
            return gatt != null && gatt.discoverServices();
        } catch (Throwable t) {
            return false;
        }
    }

    private static BluetoothGattCharacteristic findChar(String service, String chr) {
        if (gatt == null) {
            return null;
        }
        BluetoothGattService s = gatt.getService(UUID.fromString(service));
        return s == null ? null : s.getCharacteristic(UUID.fromString(chr));
    }

    /** Subscription flag + CCCD write; completion arrives on onDescriptorWrite. */
    public static boolean setNotify(String service, String chr, boolean enable) {
        try {
            BluetoothGattCharacteristic c = findChar(service, chr);
            if (c == null || !gatt.setCharacteristicNotification(c, enable)) {
                return false;
            }
            BluetoothGattDescriptor d = c.getDescriptor(CCCD);
            if (d == null) {
                return false;
            }
            // The deprecated setValue/writeDescriptor pair works on every
            // Android version; the API 33 variants do not exist below 33.
            d.setValue(enable
                    ? BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
                    : BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE);
            return gatt.writeDescriptor(d);
        } catch (Throwable t) {
            return false;
        }
    }

    /** Write with response; completion arrives on onCharacteristicWrite. */
    public static boolean writeCharacteristic(String service, String chr, byte[] value) {
        try {
            BluetoothGattCharacteristic c = findChar(service, chr);
            if (c == null) {
                return false;
            }
            c.setWriteType(BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT);
            c.setValue(value);
            return gatt.writeCharacteristic(c);
        } catch (Throwable t) {
            return false;
        }
    }

    public static void disconnect() {
        try {
            if (gatt != null) {
                gatt.disconnect();
                gatt.close();
            }
        } catch (Throwable t) {
        }
        gatt = null;
    }

    /** Keeps the screen on while the app is in the foreground. */
    public static void keepScreenOn(final Activity activity) {
        try {
            activity.runOnUiThread(new Runnable() {
                @Override
                public void run() {
                    activity.getWindow().addFlags(
                            WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
                }
            });
        } catch (Throwable t) {
        }
    }
}
