//! SMS PDU Encoding, Decoding & Concatenation Reassembly Engine (3GPP TS 23.040 / 23.038).
//! Supports 7-bit GSM default alphabet, 8-bit data, 16-bit UCS-2,
//! semi-octet address parsing, and multi-part SMS reassembly.
//! Conforms strictly to GEMINI.md systems discipline.

/// SMS Data Coding Scheme (DCS)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmsEncoding {
    Gsm7Bit,
    EightBit,
    Ucs2,
}

/// Incoming or Outgoing Parsed SMS Message
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmsMessage {
    pub sender: String,
    pub recipient: String,
    pub body: String,
    pub encoding: SmsEncoding,
    pub concat_ref: Option<u16>,
    pub concat_total: u8,
    pub concat_seq: u8,
}

/// Multipart SMS Reassembly Cache
pub struct SmsReassembler {
    pending_parts: Vec<(u16, u8, u8, String, String)>, // (ref, total, seq, sender, text)
}

impl Default for SmsReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl SmsReassembler {
    pub fn new() -> Self {
        Self {
            pending_parts: Vec::with_capacity(16),
        }
    }

    /// Add a parsed SMS segment. Returns completed full message if all segments have arrived.
    pub fn add_segment(&mut self, msg: SmsMessage) -> Option<SmsMessage> {
        let concat_ref = match msg.concat_ref {
            Some(r) => r,
            None => return Some(msg), // Standalone single-part message
        };

        if msg.concat_total <= 1 {
            return Some(msg);
        }

        // Deduplication: prevent duplicate segments (retransmissions) from corrupting the sequence
        let already_exists = self.pending_parts.iter().any(|(r, t, seq, s, _)| {
            *r == concat_ref && *t == msg.concat_total && *seq == msg.concat_seq && s == &msg.sender
        });
        if !already_exists {
            self.pending_parts.push((
                concat_ref,
                msg.concat_total,
                msg.concat_seq,
                msg.sender.clone(),
                msg.body,
            ));
        }

        // Check if all parts from 1 to concat_total are present for this (ref, sender)
        let matching: Vec<&(u16, u8, u8, String, String)> = self
            .pending_parts
            .iter()
            .filter(|(r, t, _, s, _)| *r == concat_ref && *t == msg.concat_total && s == &msg.sender)
            .collect();

        let all_present = (1..=msg.concat_total).all(|needed_seq| {
            matching.iter().any(|(_, _, seq, _, _)| *seq == needed_seq)
        });

        if all_present && matching.len() >= msg.concat_total as usize {
            let mut sorted_parts = matching;
            sorted_parts.sort_by_key(|(_, _, seq, _, _)| *seq);
            let mut full_body = String::new();
            for part in &sorted_parts {
                full_body.push_str(&part.4);
            }

            // Clean up cache for this message
            self.pending_parts
                .retain(|(r, _, _, s, _)| !(*r == concat_ref && s == &msg.sender));

            Some(SmsMessage {
                sender: msg.sender,
                recipient: msg.recipient,
                body: full_body,
                encoding: msg.encoding,
                concat_ref: Some(concat_ref),
                concat_total: msg.concat_total,
                concat_seq: 1,
            })
        } else {
            None
        }
    }
}

/// 3GPP TS 23.038 GSM 7-bit default alphabet table
pub const GSM_7BIT_TO_CHAR: [char; 128] = [
    '@', '£', '$', '¥', 'è', 'é', 'ù', 'ì', 'ò', 'Ç', '\n', 'Ø', 'ø', '\r', 'Å', 'å',
    'Δ', '_', 'Φ', 'Γ', 'Λ', 'Ω', 'Π', 'Ψ', 'Σ', 'Θ', 'Ξ', '\u{1B}', 'Æ', 'æ', 'ß', 'É',
    ' ', '!', '"', '#', '¤', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/',
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?',
    '¡', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O',
    'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', 'Ä', 'Ö', 'Ñ', 'Ü', '§',
    '¿', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o',
    'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'ä', 'ö', 'ñ', 'ü', 'à',
];

/// 3GPP TS 23.038 GSM 7-bit extension table (preceded by 0x1B escape code)
pub fn gsm_extension_to_char(code: u8) -> Option<char> {
    match code {
        0x0A => Some('\u{0C}'), // Form Feed
        0x14 => Some('^'),
        0x28 => Some('{'),
        0x29 => Some('}'),
        0x2F => Some('\\'),
        0x3C => Some('['),
        0x3D => Some('~'),
        0x3E => Some(']'),
        0x40 => Some('|'),
        0x65 => Some('€'),
        _ => None,
    }
}

pub fn char_to_gsm_extension(c: char) -> Option<u8> {
    match c {
        '\u{0C}' => Some(0x0A),
        '^' => Some(0x14),
        '{' => Some(0x28),
        '}' => Some(0x29),
        '\\' => Some(0x2F),
        '[' => Some(0x3C),
        '~' => Some(0x3D),
        ']' => Some(0x3E),
        '|' => Some(0x40),
        '€' => Some(0x65),
        _ => None,
    }
}

pub fn char_to_gsm_septet(c: char) -> Option<u8> {
    GSM_7BIT_TO_CHAR.iter().position(|&ch| ch == c).map(|p| p as u8)
}

/// Decode 7-bit septets to String using 3GPP TS 23.038 alphabet and extension codes
pub fn decode_gsm7(septets: &[u8]) -> String {
    let mut s = String::with_capacity(septets.len());
    let mut i = 0;
    while i < septets.len() {
        let code = septets[i] & 0x7F;
        if code == 0x1B && i + 1 < septets.len() {
            i += 1;
            let ext_code = septets[i] & 0x7F;
            if let Some(ch) = gsm_extension_to_char(ext_code) {
                s.push(ch);
            } else {
                s.push(' ');
            }
        } else {
            let ch = GSM_7BIT_TO_CHAR[code as usize];
            s.push(ch);
        }
        i += 1;
    }
    s
}

/// Encode String to 7-bit septets using 3GPP TS 23.038 alphabet.
/// Returns None if string contains characters outside the GSM 7-bit charset (requiring UCS-2).
pub fn encode_gsm7(text: &str) -> Option<Vec<u8>> {
    let mut septets = Vec::with_capacity(text.len());
    for c in text.chars() {
        if let Some(ext_code) = char_to_gsm_extension(c) {
            septets.push(0x1B);
            septets.push(ext_code);
        } else {
            let code = char_to_gsm_septet(c)?;
            septets.push(code);
        }
    }
    Some(septets)
}

/// Pack 7-bit septets with a bit offset and optional prefix octets (3GPP TS 23.038 / 23.040)
pub fn pack_7bit_with_bit_offset(septets: &[u8], start_bit: usize, prefix: &[u8]) -> Vec<u8> {
    let total_bits = start_bit + septets.len() * 7;
    let byte_count = total_bits.div_ceil(8);
    let mut packed = vec![0u8; byte_count];

    let prefix_len = prefix.len().min(packed.len());
    packed[..prefix_len].copy_from_slice(&prefix[..prefix_len]);

    for (i, &septet) in septets.iter().enumerate() {
        let bit_offset = start_bit + i * 7;
        let byte_idx = bit_offset / 8;
        let shift = bit_offset % 8;

        let s = septet & 0x7F;
        packed[byte_idx] |= s << shift;
        if shift > 1 && byte_idx + 1 < packed.len() {
            packed[byte_idx + 1] |= s >> (8 - shift);
        }
    }

    packed
}

/// GSM 7-bit default alphabet pack & unpack (3GPP TS 23.038)
pub fn pack_7bit(chars: &[u8]) -> Vec<u8> {
    pack_7bit_with_bit_offset(chars, 0, &[])
}

/// Unpack 7-bit septets starting at an arbitrary bit offset (handling UDH fill bits)
pub fn unpack_7bit_from_bit_offset(packed: &[u8], start_bit: usize, septet_count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(septet_count);

    for i in 0..septet_count {
        let bit_offset = start_bit + i * 7;
        let byte_idx = bit_offset / 8;
        let shift = bit_offset % 8;

        if byte_idx >= packed.len() {
            break;
        }

        let low = packed[byte_idx] >> shift;
        let high = if shift > 1 && byte_idx + 1 < packed.len() {
            packed[byte_idx + 1] << (8 - shift)
        } else {
            0
        };

        let septet = (low | high) & 0x7F;
        out.push(septet);
    }

    out
}

pub fn unpack_7bit(packed: &[u8], septet_count: usize) -> Vec<u8> {
    unpack_7bit_from_bit_offset(packed, 0, septet_count)
}

/// Format a phone number into semi-octets (3GPP TS 23.040)
pub fn encode_address_semi_octets(number: &str) -> (u8, Vec<u8>) {
    let is_intl = number.starts_with('+');
    let digits: Vec<u8> = number
        .chars()
        .filter(|c| c.is_ascii_digit())
        .map(|c| c as u8 - b'0')
        .collect();

    let _digit_count = digits.len() as u8;
    let type_of_address = if is_intl { 0x91 } else { 0x81 }; // 0x91 = International, 0x81 = National

    let mut bcd = Vec::with_capacity(digits.len().div_ceil(2));
    for chunk in digits.chunks(2) {
        let low = chunk[0];
        let high = if chunk.len() > 1 { chunk[1] } else { 0x0F };
        bcd.push(low | (high << 4));
    }

    (type_of_address, bcd)
}

/// Decode semi-octets into a phone number string
pub fn decode_address_semi_octets(bcd: &[u8], num_digits: usize, type_of_address: u8) -> String {
    let mut out = String::with_capacity(num_digits + 1);
    if type_of_address == 0x91 {
        out.push('+');
    }

    for &byte in bcd {
        let low = byte & 0x0F;
        let high = (byte >> 4) & 0x0F;

        if out.len() < num_digits + if type_of_address == 0x91 { 1 } else { 0 } && low < 10 {
            out.push((b'0' + low) as char);
        }
        if out.len() < num_digits + if type_of_address == 0x91 { 1 } else { 0 } && high < 10 {
            out.push((b'0' + high) as char);
        }
    }

    out
}

/// Encode an SMS-SUBMIT PDU for transmission
pub fn encode_sms_submit_pdu(recipient: &str, text: &str) -> Vec<u8> {
    let mut pdu = Vec::with_capacity(180);

    // 1. SMSC info length = 0 (use default SMSC from SIM)
    pdu.push(0x00);

    // 2. First octet of SMS-SUBMIT: TP-MTI = 01 (SMS-SUBMIT), TP-VPF = 10 (relative validity)
    pdu.push(0x11);

    // 3. TP-Message-Reference (0 = phone allocates)
    pdu.push(0x00);

    // 4. Destination address
    let is_intl = recipient.starts_with('+');
    let digits: Vec<u8> = recipient
        .chars()
        .filter(|c| c.is_ascii_digit())
        .map(|c| c as u8 - b'0')
        .collect();
    pdu.push(digits.len() as u8);
    pdu.push(if is_intl { 0x91 } else { 0x81 });
    for chunk in digits.chunks(2) {
        let low = chunk[0];
        let high = if chunk.len() > 1 { chunk[1] } else { 0x0F };
        pdu.push(low | (high << 4));
    }

    // 5. TP-PID (Protocol ID) = 0x00 (Standard)
    pdu.push(0x00);

    // 6. Check if text can be encoded in 7-bit GSM (including extension characters)
    if let Some(septets) = encode_gsm7(text) {
        // TP-DCS = 0x00 (GSM 7-bit default alphabet)
        pdu.push(0x00);
        // TP-VP = 0xA7 (Validity period ~24 hours)
        pdu.push(0xA7);
        // TP-UDL (User Data Length in septets)
        pdu.push(septets.len() as u8);
        // TP-UD
        let packed = pack_7bit(&septets);
        pdu.extend_from_slice(&packed);
    } else {
        // TP-DCS = 0x08 (UCS-2 16-bit)
        pdu.push(0x08);
        pdu.push(0xA7);
        let ucs2_bytes: Vec<u8> = text
            .encode_utf16()
            .flat_map(|u| u.to_be_bytes())
            .collect();
        // TP-UDL (User Data Length in bytes)
        pdu.push(ucs2_bytes.len() as u8);
        pdu.extend_from_slice(&ucs2_bytes);
    }

    pdu
}

/// Encode an SMS-DELIVER PDU (incoming network message)
pub fn encode_sms_deliver_pdu(
    sender: &str,
    text: &str,
    concat_info: Option<(u16, u8, u8)>, // (ref, total, seq)
) -> Vec<u8> {
    let mut pdu = Vec::with_capacity(180);

    // 1. SMSC info length = 0
    pdu.push(0x00);

    // 2. First octet: TP-MTI = 00 (SMS-DELIVER), TP-UDHI = 0x40 if concat
    let first_octet = if concat_info.is_some() { 0x44 } else { 0x04 };
    pdu.push(first_octet);

    // 3. Originating address
    let is_intl = sender.starts_with('+');
    let digits: Vec<u8> = sender
        .chars()
        .filter(|c| c.is_ascii_digit())
        .map(|c| c as u8 - b'0')
        .collect();
    pdu.push(digits.len() as u8);
    pdu.push(if is_intl { 0x91 } else { 0x81 });
    for chunk in digits.chunks(2) {
        let low = chunk[0];
        let high = if chunk.len() > 1 { chunk[1] } else { 0x0F };
        pdu.push(low | (high << 4));
    }

    // 4. TP-PID
    pdu.push(0x00);

    // 5. TP-DCS & TP-SCTS (7 bytes timestamp)
    let gsm_septets = encode_gsm7(text);
    let is_7bit = gsm_septets.is_some();
    pdu.push(if is_7bit { 0x00 } else { 0x08 });

    // SCTS: 26-09-22 21:00:00 +00
    pdu.extend_from_slice(&[0x62, 0x90, 0x22, 0x12, 0x00, 0x00, 0x00]);

    // 6. User Data & UDH
    if is_7bit {
        let septets = gsm_septets.unwrap();
        if let Some((cref, total, seq)) = concat_info {
            let udh = [0x05, 0x00, 0x03, (cref & 0xFF) as u8, total, seq];
            // 6 header bytes = 48 bits, 1 fill bit = 49 bits = 7 septets
            let header_septets = 7;
            let udl = header_septets + septets.len();
            pdu.push(udl as u8);

            let packed = pack_7bit_with_bit_offset(&septets, 49, &udh);
            pdu.extend_from_slice(&packed);
        } else {
            pdu.push(septets.len() as u8);
            let packed = pack_7bit(&septets);
            pdu.extend_from_slice(&packed);
        }
    } else {
        let ucs2_bytes: Vec<u8> = text
            .encode_utf16()
            .flat_map(|u| u.to_be_bytes())
            .collect();
        if let Some((cref, total, seq)) = concat_info {
            let udh = [0x05, 0x00, 0x03, (cref & 0xFF) as u8, total, seq];
            let udl = udh.len() + ucs2_bytes.len();
            pdu.push(udl as u8);
            pdu.extend_from_slice(&udh);
            pdu.extend_from_slice(&ucs2_bytes);
        } else {
            pdu.push(ucs2_bytes.len() as u8);
            pdu.extend_from_slice(&ucs2_bytes);
        }
    }

    pdu
}

/// Parse an incoming SMS-DELIVER PDU
pub fn parse_sms_deliver_pdu(pdu: &[u8]) -> Result<SmsMessage, &'static str> {
    if pdu.is_empty() {
        return Err("Empty PDU");
    }

    let mut cursor = 0;

    // 1. Skip SMSC header
    let smsc_len = pdu[cursor] as usize;
    cursor += 1 + smsc_len;
    if cursor >= pdu.len() {
        return Err("PDU truncated after SMSC");
    }

    // 2. First octet of SMS-DELIVER
    let first_octet = pdu[cursor];
    cursor += 1;
    let has_udh = (first_octet & 0x40) != 0;

    // 3. Originating address
    if cursor >= pdu.len() {
        return Err("PDU truncated at sender length");
    }
    let digit_count = pdu[cursor] as usize;
    cursor += 1;
    if cursor >= pdu.len() {
        return Err("PDU truncated before type of address");
    }
    let toa = pdu[cursor];
    cursor += 1;
    let addr_byte_len = digit_count.div_ceil(2);
    if cursor + addr_byte_len > pdu.len() {
        return Err("PDU truncated at sender address");
    }
    let sender = decode_address_semi_octets(&pdu[cursor..cursor + addr_byte_len], digit_count, toa);
    cursor += addr_byte_len;

    // 4. Protocol ID & 5. Data Coding Scheme (DCS)
    if cursor + 2 > pdu.len() {
        return Err("PDU truncated before PID/DCS");
    }
    cursor += 1; // Skip PID

    let dcs = pdu[cursor];
    cursor += 1;
    let encoding = if (dcs & 0xC0) == 0 {
        match (dcs >> 2) & 0x03 {
            0x00 => SmsEncoding::Gsm7Bit,
            0x01 => SmsEncoding::EightBit,
            0x02 => SmsEncoding::Ucs2,
            _ => SmsEncoding::Gsm7Bit,
        }
    } else if (dcs & 0xF0) == 0xF0 {
        if (dcs & 0x04) != 0 {
            SmsEncoding::EightBit
        } else {
            SmsEncoding::Gsm7Bit
        }
    } else {
        SmsEncoding::Gsm7Bit
    };

    // 6. Service Center Time Stamp (SCTS) - 7 bytes and 7. User Data Length (UDL)
    if cursor + 8 > pdu.len() {
        return Err("PDU truncated before timestamp or UDL");
    }
    cursor += 7; // Skip 7-byte timestamp

    let udl = pdu[cursor] as usize;
    cursor += 1;

    let user_data = &pdu[cursor..];

    // Check UDH for concatenation
    let mut concat_ref = None;
    let mut concat_total = 1;
    let mut concat_seq = 1;

    let body = if has_udh && !user_data.is_empty() {
        let udh_len = user_data[0] as usize;
        if 1 + udh_len > user_data.len() {
            return Err("UDH header length exceeds user data");
        }
        let mut udh_pos = 1;
        while udh_pos + 2 <= 1 + udh_len && udh_pos < user_data.len() {
            let iei = user_data[udh_pos];
            let iedl = user_data[udh_pos + 1] as usize;
            if iei == 0x00 && iedl >= 3 && udh_pos + 2 + iedl <= user_data.len() {
                // 8-bit reference
                concat_ref = Some(user_data[udh_pos + 2] as u16);
                concat_total = user_data[udh_pos + 3];
                concat_seq = user_data[udh_pos + 4];
            } else if iei == 0x08 && iedl >= 4 && udh_pos + 2 + iedl <= user_data.len() {
                // 16-bit reference
                concat_ref = Some(u16::from_be_bytes([
                    user_data[udh_pos + 2],
                    user_data[udh_pos + 3],
                ]));
                concat_total = user_data[udh_pos + 4];
                concat_seq = user_data[udh_pos + 5];
            }
            udh_pos += 2 + iedl;
        }

        let header_bytes = 1 + udh_len;
        match encoding {
            SmsEncoding::Gsm7Bit => {
                // In 3GPP TS 23.040, when UDH is present in 7-bit GSM, fill bits align to the next septet
                let header_bits = header_bytes * 8;
                let fill_bits = if header_bits.is_multiple_of(7) { 0 } else { 7 - (header_bits % 7) };
                let header_septets = (header_bits + fill_bits) / 7;
                let text_septets_count = udl.saturating_sub(header_septets);
                let septets = unpack_7bit_from_bit_offset(user_data, header_bits + fill_bits, text_septets_count);
                decode_gsm7(&septets)
            }
            SmsEncoding::Ucs2 => {
                if header_bytes < user_data.len() {
                    let text_bytes = &user_data[header_bytes..];
                    let u16_words: Vec<u16> = text_bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_be_bytes(*c))
                        .collect();
                    String::from_utf16_lossy(&u16_words)
                } else {
                    String::new()
                }
            }
            SmsEncoding::EightBit => {
                if header_bytes < user_data.len() {
                    String::from_utf8_lossy(&user_data[header_bytes..]).to_string()
                } else {
                    String::new()
                }
            }
        }
    } else {
        match encoding {
            SmsEncoding::Gsm7Bit => {
                let septets = unpack_7bit(user_data, udl);
                decode_gsm7(&septets)
            }
            SmsEncoding::Ucs2 => {
                let u16_words: Vec<u16> = user_data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_be_bytes(*c))
                    .collect();
                String::from_utf16_lossy(&u16_words)
            }
            SmsEncoding::EightBit => {
                String::from_utf8_lossy(user_data).to_string()
            }
        }
    };

    Ok(SmsMessage {
        sender,
        recipient: String::new(),
        body,
        encoding,
        concat_ref,
        concat_total,
        concat_seq,
    })
}
