<!-- SPDX-License-Identifier: MIT -->
# udev rules for the buffered accelerometer tier

Two rules. Both match the display accelerometer by sensor name (`accel_3d`), not by machine or user. ATL runs without them on the polled tier (sysfs reads, about 330 ms per sample). The stream log says which tier runs: `accelerometer tier: buffered` or `accelerometer tier: polled (<reason>)`.

| File | What it does |
|---|---|
| `71-atl-iio.rules` | `TAG+="uaccess"` gives the seat user access to `/dev/iio:deviceN`. At `add`: enables scan channels x, y, z and timestamp, sets the trigger to `<name>-dev<N>` (`accel_3d-dev0` on the owner's laptop), `buffer/length` 128 and `buffer/watermark` 1, then `buffer/enable` 1 last (`Documentation/iio/iio_devbuf.rst`). |
| `81-atl-iio-sensor-proxy.rules` | Sets `IIO_SENSOR_PROXY_TYPE=iio-poll-accel` on the device: iio-sensor-proxy reads it by sysfs and never enables, disables or frees the buffer ATL reads. Sorts after the distro's `80-iio-sensor-proxy.rules`, so its `=` replaces that rule's driver list. |

ATL writes no sysfs; the rules do. Reasons on the log line when the buffer is not used: `not-enabled` (rule 71 did not run), `node-denied` (no access to the char device), `node-busy` (another reader holds it), `no-data` (enabled, but no fresh scan), `clock` (scan stamps are not in the named clock), `layout` / `trigger` (kernel state the decoder does not read).

## Install (manual)

Order matters. iio-sensor-proxy must restart after rule 81 is in the udev database: a restart before that takes the buffer driver, and its release writes `buffer/enable` 0 (seen on the owner's laptop on the first install).

```
sudo install -m 0644 dist/udev/71-atl-iio.rules /etc/udev/rules.d/71-atl-iio.rules
sudo install -m 0644 dist/udev/81-atl-iio-sensor-proxy.rules /etc/udev/rules.d/81-atl-iio-sensor-proxy.rules
sudo udevadm verify --resolve-names=never /etc/udev/rules.d/71-atl-iio.rules /etc/udev/rules.d/81-atl-iio-sensor-proxy.rules
sudo udevadm control --reload
sudo udevadm trigger --action=add --subsystem-match=iio --attr-match=name=accel_3d   # re-runs the add rules for this device only; enables the buffer
sudo systemctl restart iio-sensor-proxy
```

Only if `buffer/enable` reads 0 after the restart, run the trigger line once more.

Verify, as the seat user:

```
cat /sys/bus/iio/devices/iio:device0/buffer/enable   # 1
getfacl /dev/iio:device0 | grep "user:$(whoami):"    # the seat user, rw-
udevadm info -q property -n /dev/iio:device0 | grep -E 'TAGS|IIO_SENSOR_PROXY_TYPE'   # TAGS has :uaccess:; IIO_SENSOR_PROXY_TYPE=iio-poll-accel
```

A distro package installs the same two files to `/usr/lib/udev/rules.d/`, not `/etc/udev/rules.d/`.
