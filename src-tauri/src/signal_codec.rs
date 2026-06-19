pub fn decode(
    data: &[u8],
    start_bit: u64,
    length: u64,
    little_endian: bool,
    signed: bool,
    factor: f64,
    offset: f64,
) -> f64 {
    let raw = extract_bits(data, start_bit, length, little_endian);
    let physical = if signed && length > 0 {
        let msb_mask = 1u64 << (length - 1);
        if raw & msb_mask != 0 {
            let sign_extended = raw | !((1u64 << length) - 1);
            sign_extended as i64 as f64
        } else {
            raw as f64
        }
    } else {
        raw as f64
    };
    physical * factor + offset
}

pub fn encode(data: &mut [u8], value: f64, start_bit: u64, length: u64, little_endian: bool, factor: f64, offset: f64) {
    let raw = ((value - offset) / factor).round() as i64;
    let mask = if length >= 64 { u64::MAX } else { (1u64 << length) - 1 };
    let raw_u64 = (raw as u64) & mask;
    pack_bits(data, raw_u64, start_bit, length, little_endian);
}

fn extract_bits(data: &[u8], start_bit: u64, length: u64, little_endian: bool) -> u64 {
    let mut raw = 0u64;
    if little_endian {
        for i in 0..length {
            let bit_pos = start_bit + i;
            let byte_idx = (bit_pos / 8) as usize;
            let bit_in_byte = (bit_pos % 8) as u32;
            if byte_idx < data.len() {
                raw |= (((data[byte_idx] >> bit_in_byte) & 1) as u64) << i;
            }
        }
    } else {
        // Motorola/Big-endian: start_bit is MSB in DBC bit numbering
        let mut bit_pos = start_bit;
        for i in 0..length {
            let byte_idx = (bit_pos / 8) as usize;
            let bit_in_byte = (bit_pos % 8) as u32;
            if byte_idx < data.len() {
                raw |= (((data[byte_idx] >> bit_in_byte) & 1) as u64) << (length - 1 - i);
            }
            if bit_pos % 8 == 0 {
                bit_pos = bit_pos.saturating_add(15);
            } else {
                bit_pos -= 1;
            }
        }
    }
    raw
}

fn pack_bits(data: &mut [u8], raw: u64, start_bit: u64, length: u64, little_endian: bool) {
    if little_endian {
        for i in 0..length {
            let bit_pos = start_bit + i;
            let byte_idx = (bit_pos / 8) as usize;
            let bit_in_byte = (bit_pos % 8) as u32;
            if byte_idx < data.len() {
                if (raw >> i) & 1 == 1 {
                    data[byte_idx] |= 1 << bit_in_byte;
                } else {
                    data[byte_idx] &= !(1u8 << bit_in_byte);
                }
            }
        }
    } else {
        let mut bit_pos = start_bit;
        for i in 0..length {
            let byte_idx = (bit_pos / 8) as usize;
            let bit_in_byte = (bit_pos % 8) as u32;
            if byte_idx < data.len() {
                if (raw >> (length - 1 - i)) & 1 == 1 {
                    data[byte_idx] |= 1 << bit_in_byte;
                } else {
                    data[byte_idx] &= !(1u8 << bit_in_byte);
                }
            }
            if bit_pos % 8 == 0 {
                bit_pos = bit_pos.saturating_add(15);
            } else {
                bit_pos -= 1;
            }
        }
    }
}
