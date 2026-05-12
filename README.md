# SatWatch-Discord-Bot

A Discord bot to send CubeSat updates to Discord (beacon data, overpass forecasts).

- **TLE source**: [SatNOGS DB](https://db.satnogs.org/) public API (always fresh)
- **Propagator**: [satkit](https://docs.rs/satkit) SGP4 (`satkit::sgp4`)
- **Frame transforms**: TEME → ITRF → ENU via `satkit::frametransform`
- **Discord**: [Serenity](https://github.com/serenity-rs/serenity) with slash commands

---

## Slash commands

### `/passes`
Predict upcoming satellite passes over a ground station.

| Option | Required | Description |
|---|---|---|
| `norad_id` | ✅ | NORAD catalog number (e.g. `69015` for FrontierSat) |
| `lat` | ✅ | Ground station latitude in decimal degrees |
| `lon` | ✅ | Ground station longitude in decimal degrees |
| `elevation_m` | ❌ | Altitude above ellipsoid in metres (default 0) |
| `hours` | ❌ | Search window in hours, 1–72 (default 24) |
| `min_elev` | ❌ | Minimum peak elevation to report in degrees (default 5) |
| `station_name` | ❌ | Label for the station (default: coords) |

**Example:**
```
/passes norad_id:69015 lat:51.0447 lon:-114.0719 elevation_m:1045 station_name:Calgary hours:24 min_elev:10
```

### `/tle`
Fetch and display the current TLE from SatNOGS.

| Option | Required | Description |
|---|---|---|
| `norad_id` | ✅ | NORAD catalog number |

---

---

## Architecture

```
src/
├── main.rs      – Discord bot entrypoint, slash command registration & routing
├── satnogs.rs   – Async HTTP fetch of TLE data from SatNOGS DB API
└── passes.rs    – SGP4 propagation + pass prediction algorithm
```

### Pass prediction algorithm (`passes.rs`)

1. **Coarse scan** – step through time in 30-second increments, computing
   elevation at each step via:
   - `satkit::sgp4::sgp4()` → TEME position (km → m)
   - `satkit::frametransform::qteme2itrf()` → rotate to ITRF
   - `ITRFCoord::to_enu()` → East-North-Up vector at the ground station
   - `el = atan2(Up, √(E²+N²))`, `az = atan2(E, N)`

2. **Rising edge detected** → binary-search bisect (16 iterations, ~1 ms resolution)
   to find precise AOS time.

3. **Fine scan** inside the pass (10-second steps) to find maximum elevation.

4. **Falling edge detected** → bisect for precise LOS time.

5. Passes filtered to those where peak elevation ≥ `min_elev`.

---

## Notable NORAD IDs

| Satellite | NORAD | SatNOGS |
|---|---|---|
| FrontierSat | 69015 | [GGCH-4346](https://db.satnogs.org/satellite/GGCH-4346-1583-9419-5634) |
| ISS | 25544 | [ZARYA](https://db.satnogs.org/satellite/AAAA-0000-0000-0000-0001) |
| NOAA-18 | 28654 | — |
| NOAA-19 | 33591 | — |
| Meteor-M N2-3 | 57166 | — |
