#!/bin/sh
# Rebuild the embedded dex shims from the Java sources.
#
#   assets/ble-bridge.dex       <- BleBridge.java
#   assets/location-bridge.dex  <- LocationBridge.java
#
# Each is loaded independently at runtime (BLE worker / GPS source) and embedded
# via its own include_bytes!, so they are emitted as separate dex files.
#
# Needs the Android SDK: platforms/android-33 and build-tools/34.0.0 (adjust
# below if yours differ). Run after any change to the .java files and commit the
# resulting dexes - the APK build itself has no Java step.
set -e
cd "$(dirname "$0")"

SDK="${ANDROID_HOME:-$HOME/Android}"
JAR="$SDK/platforms/android-33/android.jar"
D8="$SDK/build-tools/34.0.0/d8"

rm -rf classes dex-ble dex-loc
mkdir -p classes dex-ble dex-loc ../assets

javac -source 1.8 -target 1.8 -bootclasspath "$JAR" -d classes \
    BleBridge.java LocationBridge.java

"$D8" --release --lib "$JAR" --min-api 23 --output dex-ble \
    classes/rs/gps/gui/BleBridge*.class
mv dex-ble/classes.dex ../assets/ble-bridge.dex

"$D8" --release --lib "$JAR" --min-api 23 --output dex-loc \
    classes/rs/gps/gui/LocationBridge*.class
mv dex-loc/classes.dex ../assets/location-bridge.dex

rm -rf classes dex-ble dex-loc
echo "wrote assets/ble-bridge.dex and assets/location-bridge.dex"
