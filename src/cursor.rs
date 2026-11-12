//! Cursor bitmaps are transported separately from video and displayed locally.
use crate::protocol::CursorShape;
use anyhow::{ensure, Result};
use windows::Win32::{Graphics::Gdi::*, UI::WindowsAndMessaging::*};

struct Bitmap(HBITMAP);
impl Drop for Bitmap {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.0 .0));
            }
        }
    }
}

pub fn snapshot(handle: usize) -> Result<CursorShape> {
    unsafe {
        let mut icon = ICONINFO::default();
        GetIconInfo(HICON(handle as *mut _), &mut icon)?;
        let color = Bitmap(icon.hbmColor);
        let mask = Bitmap(icon.hbmMask);
        let mut bm = BITMAP::default();
        ensure!(
            GetObjectW(
                HGDIOBJ(mask.0 .0),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut _ as *mut _)
            ) != 0,
            "cursor mask info"
        );
        let kind = u32::from(!color.0.is_invalid());
        let width = bm.bmWidth as u32;
        let height = bm.bmHeight as u32 / if kind == 0 { 2 } else { 1 };
        ensure!(
            width > 0 && height > 0 && width <= 512 && height <= 512,
            "cursor dimensions"
        );
        let mut bytes = vec![];
        if kind == 1 {
            bytes.resize((width * height * 4) as usize, 0);
            let mut info = bitmap_info(width, height);
            let dc = GetDC(None);
            let lines = GetDIBits(
                dc,
                color.0,
                0,
                height,
                Some(bytes.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            );
            ReleaseDC(None, dc);
            ensure!(lines == height as i32, "reading color cursor");
        }
        let mask_len = (bm.bmWidthBytes * bm.bmHeight) as usize;
        let offset = bytes.len();
        bytes.resize(offset + mask_len, 0);
        ensure!(
            GetBitmapBits(mask.0, mask_len as i32, bytes[offset..].as_mut_ptr().cast())
                == mask_len as i32,
            "reading cursor mask"
        );
        Ok(CursorShape {
            kind,
            width,
            height,
            pitch: bm.bmWidthBytes as u32,
            hot_x: icon.xHotspot as i32,
            hot_y: icon.yHotspot as i32,
            bytes,
        })
    }
}

pub struct NativeCursor(pub HCURSOR);
impl Drop for NativeCursor {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyCursor(self.0);
        }
    }
}
impl NativeCursor {
    pub fn from_shape(shape: &CursorShape) -> Result<Self> {
        // Validate in-memory shapes with the same checks used on the wire.
        let mut wire = vec![];
        shape.encode(&mut wire);
        CursorShape::decode(&mut crate::protocol::Reader::new(&wire))?;
        unsafe {
            let color_len = if shape.kind == 1 {
                (shape.width * shape.height * 4) as usize
            } else {
                0
            };
            let color = if shape.kind == 1 {
                let info = bitmap_info(shape.width, shape.height);
                let mut bits = std::ptr::null_mut();
                let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
                let owned = Bitmap(bitmap);
                ensure!(!bits.is_null(), "cursor bitmap allocation");
                std::ptr::copy_nonoverlapping(shape.bytes.as_ptr(), bits as *mut u8, color_len);
                owned
            } else {
                Bitmap(HBITMAP::default())
            };
            let mask = Bitmap(CreateBitmap(
                shape.width as i32,
                (shape.height * if shape.kind == 0 { 2 } else { 1 }) as i32,
                1,
                1,
                Some(shape.bytes[color_len..].as_ptr().cast()),
            ));
            ensure!(!mask.0.is_invalid(), "cursor mask allocation");
            let icon = CreateIconIndirect(&ICONINFO {
                fIcon: false.into(),
                xHotspot: shape.hot_x as u32,
                yHotspot: shape.hot_y as u32,
                hbmMask: mask.0,
                hbmColor: color.0,
            })?;
            Ok(Self(HCURSOR(icon.0)))
        }
    }
}
fn bitmap_info(width: u32, height: u32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn system_cursor_survives_bitmap_roundtrip() {
        unsafe {
            for id in [IDC_ARROW, IDC_IBEAM, IDC_CROSS, IDC_HAND] {
                let original = LoadCursorW(None, id).unwrap();
                let shape = snapshot(original.0 as usize).unwrap();
                let restored = NativeCursor::from_shape(&shape).unwrap();
                assert_eq!(snapshot(restored.0 .0 as usize).unwrap(), shape);
            }
        }
    }
}
