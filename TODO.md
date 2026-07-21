~~Import gps / sensor data?~~

Persist beacon notify interval in firmware flash?

Set font?

Set interval for accelerometer, gps, BLE updates. 

Need BLE to be more robust?

User stories

Toggle button to continually attempt to wake.

~~Tracking mode and north up mode should use the accelerometer as well but at a lower hz make this a setting. Pointing the arrow on the maker like north up mode.~~
Compass now runs in every mode: full rate for heading-up, `compass.arrow_hz`
(default 4) for the marker arrow in north-up and tracking. `compass.marker_arrow`
turns the latter off.

Should be a force reconnect from scratch button.

More statuses in the app.

Toml color theme control. (partly done)
`[colors] outline` and a new `[ui]` table (`ok`, `error`, `pulse`) replaced the
hardcoded colors. The rest of the pages still follow the egui theme.

More color changes.

App needs to read the stats over usb from ESP as well.

Have receiver mode to get info.

Disconnect button or toggle (Must force disconnect).

Need to configure advertising window.

Add some memory for basic settings (Toml). Automatic if nothing present.
Default path for toml.

Remove some buttons on top bar? (Need to choose)

Red pulsing icons at the top should be only pulsing for a time if pressed when not valid. Otherwise greyed out.

Maybe make top bar a dropdown?

Text goes behind the page menu dropdown button.

~~Need to be able to handel multiple ESP's BLE at once. (Probably one at a time? Names? Select from a list?)~~
One at a time, picked from a scanned list, named in the app config (`[ble.names]`).

Change `gps-config.toml` name.

Beacon track is shared across boards, so the drawn path can span two of them after switching. Split it per board? (Points model change)

Optimize.

GPS BLE mesh? 1 central shares coords with others over BLE?

Edit dialog is too small.

Better color theme. Something visible in poor conditions.

Clean up the pages.

~~Show/hide path toggle instead of delete paths on map bar.~~
~~Setting for toggling central path as well.~~
The bar button is a session-only master switch over both paths (`show_paths`);
`[track] show_path` / `[ble] show_path` say which ones a shown map draws. The
line to the beacon and its distance stay. Discarding points moved to Settings.
