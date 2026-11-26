use anyhow::{Result, ensure};

#[cfg(windows)]
struct Apartment;
#[cfg(windows)]
impl Apartment {
    fn new() -> Result<Self> {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        }
        Ok(Self)
    }
}
#[cfg(windows)]
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
}

pub(super) fn open_link(raw: &str) -> Result<()> {
    let url = super::validate_url(raw)?;
    #[cfg(windows)]
    {
        use windows::{
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            core::{PCWSTR, w},
        };
        let _apartment = Apartment::new()?;
        let address: Vec<u16> = format!("microsoft-edge:{url}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(address.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        let code = result.0 as isize;
        ensure!(
            code > 32,
            "{} ({code})",
            crate::i18n::tr("无法打开 Microsoft Edge")
        );
    }
    #[cfg(not(windows))]
    std::process::Command::new("microsoft-edge")
        .arg(url.as_str())
        .spawn()?;
    Ok(())
}

#[cfg(windows)]
pub(super) fn video_thumbnail(path: &str) -> Result<image::DynamicImage> {
    use windows::{
        Win32::{
            Foundation::SIZE,
            Graphics::Gdi::{
                BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS,
                DeleteDC, DeleteObject, GetDIBits, GetObjectW, HBITMAP, HDC,
            },
            UI::Shell::{
                IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_THUMBNAILONLY,
            },
        },
        core::PCWSTR,
    };
    struct Bitmap(HBITMAP);
    impl Drop for Bitmap {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteObject(self.0.into());
            }
        }
    }
    struct Dc(HDC);
    impl Drop for Dc {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }
    let _apartment = Apartment::new()?;
    let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None)? };
    // THUMBNAILONLY prevents a file-type icon from masquerading as a video frame.
    let bitmap =
        Bitmap(unsafe { factory.GetImage(SIZE { cx: 768, cy: 480 }, SIIGBF_THUMBNAILONLY)? });
    let mut description = BITMAP::default();
    ensure!(
        unsafe {
            GetObjectW(
                bitmap.0.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some((&mut description as *mut BITMAP).cast()),
            )
        } > 0,
        "Cannot read video thumbnail"
    );
    let width = description.bmWidth;
    let height = description.bmHeight;
    ensure!(
        width > 0 && width <= 4096 && height > 0 && height <= 4096,
        "Invalid thumbnail dimensions"
    );
    let dc = Dc(unsafe { CreateCompatibleDC(None) });
    ensure!(!dc.0.is_invalid(), "Cannot create thumbnail DC");
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    ensure!(
        unsafe {
            GetDIBits(
                dc.0,
                bitmap.0,
                0,
                height as u32,
                Some(pixels.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        } == height,
        "Cannot decode video thumbnail"
    );
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    let image = image::RgbaImage::from_raw(width as u32, height as u32, pixels)
        .ok_or_else(|| anyhow::anyhow!("Invalid thumbnail buffer"))?;
    Ok(image::DynamicImage::ImageRgba8(image))
}

#[cfg(not(windows))]
pub(super) fn video_thumbnail(_: &str) -> Result<image::DynamicImage> {
    anyhow::bail!("Video thumbnails require Windows")
}
