//! Android GNSS HAL & NMEA-0183 GPS Service.
//! Interfaces with android.hardware.gnss@1.0-2.1 and AIDL IGnss,
//! tracks multi-constellation satellites (GPS, GLONASS, Galileo, BeiDou, QZSS),
//! and generates standard NMEA-0183 sentences ($GPRMC, $GPGGA, $GPGSA, $GPGSV)
//! with verified checksums for gpsd and geoclue2.
//! Conforms strictly to GEMINI.md systems discipline.

/// Satellite Constellation Type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GnssConstellation {
    Unknown = 0,
    Gps = 1,
    Sbas = 2,
    Glonass = 3,
    Qzss = 4,
    Beidou = 5,
    Galileo = 6,
    Irnss = 7,
}

/// Satellite Status Information
#[derive(Debug, Clone, PartialEq)]
pub struct SatelliteInfo {
    pub svid: u32,
    pub constellation: GnssConstellation,
    pub snr_dbhz: f32,
    pub elevation_deg: f32,
    pub azimuth_deg: f32,
    pub has_ephemeris: bool,
    pub has_almanac: bool,
    pub used_in_fix: bool,
}

/// Geographic Position Fix
#[derive(Debug, Clone, PartialEq)]
pub struct GnssLocation {
    pub latitude: f64,    // degrees (-90.0 .. +90.0)
    pub longitude: f64,   // degrees (-180.0 .. +180.0)
    pub altitude_m: f64,  // meters above WGS84 ellipsoid
    pub speed_mps: f32,   // meters per second
    pub bearing_deg: f32, // degrees (0.0 .. 360.0)
    pub horizontal_accuracy_m: f32,
    pub vertical_accuracy_m: f32,
    pub timestamp_ms: u64,
}

/// Calculate standard NMEA XOR checksum for a sentence body (excluding '$' and '*')
pub fn calculate_nmea_checksum(sentence: &str) -> u8 {
    let mut checksum: u8 = 0;
    for b in sentence.bytes() {
        checksum ^= b;
    }
    checksum
}

/// Android GNSS Hardware Abstraction & NMEA Streamer
pub struct GnssService {
    pub active: bool,
    pub has_fix: bool,
    pub current_location: Option<GnssLocation>,
    pub satellites: Vec<SatelliteInfo>,
    pub fix_counter: u64,
}

impl Default for GnssService {
    fn default() -> Self {
        Self::new()
    }
}

impl GnssService {
    pub fn new() -> Self {
        Self {
            active: true,
            has_fix: false,
            current_location: None,
            satellites: Vec::with_capacity(32),
            fix_counter: 0,
        }
    }

    /// Set simulated or real location fix.
    ///
    /// H26: validates the fix so GGA stays numeric and within the NMEA-0183
    /// 82-character limit; out-of-range or non-finite input is rejected
    /// instead of emitting an invalid sentence.
    pub fn update_location(&mut self, loc: GnssLocation) -> Result<(), &'static str> {
        if !loc.latitude.is_finite() || !(-90.0..=90.0).contains(&loc.latitude) {
            return Err("latitude out of range [-90, 90]");
        }
        if !loc.longitude.is_finite() || !(-180.0..=180.0).contains(&loc.longitude) {
            return Err("longitude out of range [-180, 180]");
        }
        if !loc.altitude_m.is_finite() {
            return Err("altitude must be finite");
        }
        if !loc.speed_mps.is_finite() || !loc.bearing_deg.is_finite() {
            return Err("speed/bearing must be finite");
        }
        self.current_location = Some(loc);
        self.has_fix = true;
        self.fix_counter += 1;
        Ok(())
    }

    /// Format latitude in NMEA DDMM.MMMM format (with 60.0000 carry).
    fn format_nmea_lat(lat: f64) -> (String, char) {
        let dir = if lat >= 0.0 { 'N' } else { 'S' };
        // Round total minutes to 4dp first, then carry 60.0000 into degrees.
        let total_1e4 = (lat.abs() * 60.0 * 1.0e4).round() as u64;
        let deg = (total_1e4 / 600_000) as u32;
        let min_1e4 = (total_1e4 % 600_000) as u32;
        let min = min_1e4 as f64 / 1.0e4;
        (format!("{:02}{:07.4}", deg, min), dir)
    }

    /// Format longitude in NMEA DDDMM.MMMM format (with 60.0000 carry).
    fn format_nmea_lon(lon: f64) -> (String, char) {
        let dir = if lon >= 0.0 { 'E' } else { 'W' };
        let total_1e4 = (lon.abs() * 60.0 * 1.0e4).round() as u64;
        let deg = (total_1e4 / 600_000) as u32;
        let min_1e4 = (total_1e4 % 600_000) as u32;
        let min = min_1e4 as f64 / 1.0e4;
        (format!("{:03}{:07.4}", deg, min), dir)
    }

    /// Days-from-civil (Hinnant) for epoch-ms -> UTC civil date. No deps.
    fn civil_from_epoch_ms(ms: u64) -> (i64, u32, u32, u32, u32, u32) {
        let days = (ms / 86_400_000) as i64;
        let ms_of_day = (ms % 86_400_000) as u32;
        // Convert days since 1970-01-01 to civil date.
        let z = days + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = (z - era * 146097) as u32;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if m <= 2 { y + 1 } else { y };
        let hh = ms_of_day / 3_600_000;
        let mi = (ms_of_day % 3_600_000) / 60_000;
        let ss = (ms_of_day % 60_000) / 1000;
        (year, m, d, hh, mi, ss)
    }

    /// Generate standard $GPRMC (Recommended Minimum Navigation Information) sentence
    pub fn generate_gprmc(&self) -> Option<String> {
        let loc = self.current_location.as_ref()?;
        let (lat_str, lat_dir) = Self::format_nmea_lat(loc.latitude);
        let (lon_str, lon_dir) = Self::format_nmea_lon(loc.longitude);

        let speed_knots = loc.speed_mps * 1.94384;
        // H14: derive UTC time/date from timestamp_ms instead of hardcoded literals.
        // Clamp absurd altitude/speed inputs elsewhere; time always succeeds.
        let (y, mo, d, hh, mi, ss) = Self::civil_from_epoch_ms(loc.timestamp_ms);
        let frac = ((loc.timestamp_ms % 1000) / 10) as u32;
        let body = format!(
            "GPRMC,{:02}{:02}{:02}.{:02},A,{},{},{},{},{:.1},{:.1},{:02}{:02}{:02},,,A",
            hh,
            mi,
            ss,
            frac,
            lat_str,
            lat_dir,
            lon_str,
            lon_dir,
            speed_knots,
            loc.bearing_deg,
            d,
            mo,
            (y % 100).abs()
        );

        let csum = calculate_nmea_checksum(&body);
        Some(format!("${}*{:02X}\r\n", body, csum))
    }

    /// Generate standard $GPGGA (Global Positioning System Fix Data) sentence
    pub fn generate_gpgga(&self) -> Option<String> {
        let loc = self.current_location.as_ref()?;
        let (lat_str, lat_dir) = Self::format_nmea_lat(loc.latitude);
        let (lon_str, lon_dir) = Self::format_nmea_lon(loc.longitude);

        // H15: report the true satellite count; quality follows fix state.
        let num_used = self.satellites.iter().filter(|s| s.used_in_fix).count() as u32;
        // GGA field 6: 0 = invalid, 1 = GPS fix.
        let quality = if num_used >= 3 && self.has_fix { 1 } else { 0 };
        // H14: time from timestamp_ms.
        let (_, _, _, hh, mi, ss) = Self::civil_from_epoch_ms(loc.timestamp_ms);
        let frac = ((loc.timestamp_ms % 1000) / 10) as u32;
        // H26: clamp non-finite/huge altitude to keep the sentence <= 82 chars
        // and numeric.
        let alt = if loc.altitude_m.is_finite() {
            loc.altitude_m.clamp(-9999.9, 99999.9)
        } else {
            0.0
        };
        let body = format!(
            "GPGGA,{:02}{:02}{:02}.{:02},{},{},{},{},{},{:02},1.0,{:.1},M,-34.0,M,,",
            hh, mi, ss, frac, lat_str, lat_dir, lon_str, lon_dir, quality, num_used, alt
        );

        let csum = calculate_nmea_checksum(&body);
        Some(format!("${}*{:02X}\r\n", body, csum))
    }

    /// Generate standard $GPGSA (GNSS DOP and Active Satellites) sentence.
    ///
    /// H25: built from the actual constellation state — fix mode follows
    /// `has_fix` plus the used-in-fix count, and the 12 SVN slots list the
    /// real used SVNs — so GSA never contradicts GSV in the same burst.
    pub fn generate_gpgsa(&self) -> String {
        let used: Vec<u32> = self
            .satellites
            .iter()
            .filter(|s| s.used_in_fix)
            .map(|s| s.svid)
            .take(12)
            .collect();
        let fix = if self.has_fix && used.len() >= 3 {
            3
        } else {
            1
        };
        let mut body = format!("GPGSA,A,{}", fix);
        for i in 0..12 {
            if i < used.len() {
                body.push_str(&format!(",{:02}", used[i]));
            } else {
                body.push(',');
            }
        }
        body.push_str(",1.8,1.0,1.5");
        let csum = calculate_nmea_checksum(&body);
        format!("${}*{:02X}\r\n", body, csum)
    }

    /// Generate standard $GPGSV (GNSS Satellites in View) sentences
    pub fn generate_gpgsv(&self) -> Vec<String> {
        if self.satellites.is_empty() {
            let body = "GPGSV,1,1,0";
            let csum = calculate_nmea_checksum(body);
            return vec![format!("${}*{:02X}\r\n", body, csum)];
        }

        let total_sats = self.satellites.len();
        let total_sentences = total_sats.div_ceil(4);
        let mut sentences = Vec::with_capacity(total_sentences);

        for (idx, chunk) in self.satellites.chunks(4).enumerate() {
            let sentence_num = idx + 1;
            let mut body = format!("GPGSV,{},{},{}", total_sentences, sentence_num, total_sats);
            for sat in chunk {
                body.push_str(&format!(
                    ",{:02},{:02},{:03},{:02}",
                    sat.svid, sat.elevation_deg as u32, sat.azimuth_deg as u32, sat.snr_dbhz as u32
                ));
            }
            let csum = calculate_nmea_checksum(&body);
            sentences.push(format!("${}*{:02X}\r\n", body, csum));
        }

        sentences
    }

    /// Generate complete NMEA burst for gpsd / geoclue2
    pub fn generate_nmea_burst(&self) -> String {
        let mut burst = String::with_capacity(1024);
        if let Some(rmc) = self.generate_gprmc() {
            burst.push_str(&rmc);
        }
        if let Some(gga) = self.generate_gpgga() {
            burst.push_str(&gga);
        }
        burst.push_str(&self.generate_gpgsa());
        for gsv in self.generate_gpgsv() {
            burst.push_str(&gsv);
        }
        burst
    }
}
