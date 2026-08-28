use windows_sys::Win32::System::{
    StationsAndDesktops::{
        CloseDesktop, DESKTOP_READOBJECTS, DESKTOP_WRITEOBJECTS, GetThreadDesktop,
        OpenInputDesktop, SetThreadDesktop,
    },
    Threading::GetCurrentThreadId,
};

pub struct InputDesktopGuard {
    original: windows_sys::Win32::System::StationsAndDesktops::HDESK,
    input: windows_sys::Win32::System::StationsAndDesktops::HDESK,
}

impl InputDesktopGuard {
    pub fn enter() -> Result<Self, String> {
        unsafe {
            let original = GetThreadDesktop(GetCurrentThreadId());
            if original.is_null() {
                return Err(format!(
                    "GetThreadDesktop failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let input = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS);
            if input.is_null() {
                return Err(format!(
                    "OpenInputDesktop failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if SetThreadDesktop(input) == 0 {
                let error = std::io::Error::last_os_error();
                CloseDesktop(input);
                return Err(format!("SetThreadDesktop failed: {error}"));
            }
            Ok(Self { original, input })
        }
    }
}

impl Drop for InputDesktopGuard {
    fn drop(&mut self) {
        unsafe {
            if SetThreadDesktop(self.original) != 0 {
                CloseDesktop(self.input);
            }
        }
    }
}
