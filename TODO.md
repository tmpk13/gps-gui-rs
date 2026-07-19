Make dotted line for tracked path (beacon path is dashed now; phone track still solid)
Read from sd?
Import gps / sensor data

Store tiles offline on mobile

Test esp32c3-gps on hardware (flash, UART GPS module, BLE from phone + desktop)
Persist beacon notify interval in firmware flash?

Config page? Edit button confirm.

Center top bar. Desktop

Heart Beat on marker for esp? Or number of seconds since last message.

Add hold to center on different markers


[done] Option in settings to show distance between user and beacons next to line between. Text should be above the line. (mi/ft or km/m; label above the line midpoint)

[done] Tracking mode: Keep user and marker both in frame with some margin. (track bar button; cycles beacons then exits; heading button hidden while tracking; user near bottom, beacon near top)

[done] Independent overlay sizes ([sizes] in toml) and dotted user-beacon line ([distance] dotted).

Set interval for accelerometer, gps, BLE updates. 
Look into lowering power useage for BLE. Minimum req.

Make sure accelerometer is only on/used when in heading mode.

Set font?

Remove toml veiw from settings change this to have a generate file and reset to defaults button.

[done] Make page menu % size. (rows sized off the trigger icon, itself a fraction of the screen)