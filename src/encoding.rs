use crate::process::OutputEncoding;

pub struct TextDecoder {
    encoding: OutputEncoding,
    pending: Vec<u8>,
}

impl TextDecoder {
    pub fn new(encoding: OutputEncoding) -> Self {
        Self {
            encoding,
            pending: Vec::new(),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let split = complete_prefix_len(&self.pending, self.encoding);
        let complete = self.pending.drain(..split).collect::<Vec<_>>();
        decode(&complete, self.encoding)
    }

    pub fn finish(&mut self) -> String {
        let remaining = std::mem::take(&mut self.pending);
        decode(&remaining, self.encoding)
    }
}

fn complete_prefix_len(bytes: &[u8], encoding: OutputEncoding) -> usize {
    match encoding {
        OutputEncoding::Utf8 => match std::str::from_utf8(bytes) {
            Ok(_) => bytes.len(),
            Err(err) if err.error_len().is_none() => err.valid_up_to(),
            Err(_) => bytes.len(),
        },
        OutputEncoding::Oem => code_page_prefix_len(bytes, oem_code_page()),
        OutputEncoding::Ansi => code_page_prefix_len(bytes, ansi_code_page()),
    }
}

#[cfg(windows)]
fn code_page_prefix_len(bytes: &[u8], code_page: u32) -> usize {
    use windows_sys::Win32::Globalization::IsDBCSLeadByteEx;

    match bytes.last() {
        Some(byte) if unsafe { IsDBCSLeadByteEx(code_page, *byte) } != 0 => bytes.len() - 1,
        _ => bytes.len(),
    }
}

#[cfg(not(windows))]
fn code_page_prefix_len(bytes: &[u8], _code_page: u32) -> usize {
    bytes.len()
}

fn decode(bytes: &[u8], encoding: OutputEncoding) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    match encoding {
        OutputEncoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        OutputEncoding::Oem => decode_code_page(bytes, oem_code_page()),
        OutputEncoding::Ansi => decode_code_page(bytes, ansi_code_page()),
    }
}

pub fn decode_all(bytes: &[u8], encoding: OutputEncoding) -> String {
    let mut decoder = TextDecoder::new(encoding);
    let mut text = decoder.push(bytes);
    text.push_str(&decoder.finish());
    text
}

pub fn is_boundary(bytes: &[u8], offset: usize, encoding: OutputEncoding) -> bool {
    if offset > bytes.len() {
        return false;
    }
    match encoding {
        OutputEncoding::Utf8 => offset == bytes.len() || bytes[offset] & 0b1100_0000 != 0b1000_0000,
        OutputEncoding::Oem => code_page_boundary(bytes, offset, oem_code_page()),
        OutputEncoding::Ansi => code_page_boundary(bytes, offset, ansi_code_page()),
    }
}

fn code_page_boundary(bytes: &[u8], offset: usize, code_page: u32) -> bool {
    let mut index = 0;
    while index < offset {
        index += if is_lead_byte(bytes[index], code_page) {
            2
        } else {
            1
        };
    }
    index == offset
}

#[cfg(windows)]
fn is_lead_byte(byte: u8, code_page: u32) -> bool {
    unsafe { windows_sys::Win32::Globalization::IsDBCSLeadByteEx(code_page, byte) != 0 }
}

#[cfg(not(windows))]
fn is_lead_byte(_byte: u8, _code_page: u32) -> bool {
    false
}

#[cfg(windows)]
fn oem_code_page() -> u32 {
    unsafe { windows_sys::Win32::Globalization::GetOEMCP() }
}

#[cfg(not(windows))]
fn oem_code_page() -> u32 {
    65001
}

#[cfg(windows)]
fn ansi_code_page() -> u32 {
    unsafe { windows_sys::Win32::Globalization::GetACP() }
}

#[cfg(not(windows))]
fn ansi_code_page() -> u32 {
    65001
}

#[cfg(windows)]
fn decode_code_page(bytes: &[u8], code_page: u32) -> String {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;

    let input_len = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
    let wide_len = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            input_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if wide_len <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut wide = vec![0u16; wide_len as usize];
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            input_len,
            wide.as_mut_ptr(),
            wide_len,
        )
    };
    if written <= 0 {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        String::from_utf16_lossy(&wide[..written as usize])
    }
}

#[cfg(not(windows))]
fn decode_code_page(bytes: &[u8], _code_page: u32) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_decoder_preserves_characters_split_between_chunks() {
        let bytes = "搜狗".as_bytes();
        let mut decoder = TextDecoder::new(OutputEncoding::Utf8);
        assert_eq!(decoder.push(&bytes[..2]), "");
        assert_eq!(decoder.push(&bytes[2..4]), "搜");
        assert_eq!(decoder.push(&bytes[4..]), "狗");
        assert_eq!(decoder.finish(), "");
    }
}
