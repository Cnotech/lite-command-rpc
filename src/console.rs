#[cfg(windows)]
pub fn set_title(title: &str) -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleTitleW;

    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    if unsafe { SetConsoleTitleW(title.as_ptr()) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn set_title(_title: &str) -> std::io::Result<()> {
    Ok(())
}
