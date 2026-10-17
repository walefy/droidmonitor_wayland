/// Dynamic EDID 1.4 128-byte generator for arbitrary resolutions and refresh rates.
/// Specifically configured as a strictly Fixed-Frequency (non-VRR) display so GPU drivers
/// (`amdgpu` / `i915` / `nouveau`) never trigger `dm_handle_vmin_vmax_update` or FreeSync
/// AUX timeouts on virtual/forced DisplayPort connectors.
pub fn generate_edid(width: u16, height: u16, refresh_hz: u16) -> [u8; 128] {
    let width = width.clamp(640, 4095);
    let height = height.clamp(480, 4095);
    let refresh_hz = refresh_hz.clamp(30, 144);

    let mut edid = [0u8; 128];

    // 1. Fixed EDID 1.4 header (8 bytes)
    edid[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);

    // 2. Vendor & Product ID: "DRM" -> D(4), R(18), M(13)
    edid[8] = 0x12;
    edid[9] = 0x4D;
    let prod_code = width ^ height.rotate_left(4);
    edid[10] = (prod_code & 0xFF) as u8;
    edid[11] = (prod_code >> 8) as u8;
    edid[12..16].copy_from_slice(&[0x01, 0x00, 0x00, 0x00]);
    edid[16] = 1;
    edid[17] = 36;

    // 3. EDID Structure Version & Revision: 1.3 (strict fixed timing, no VRR/Range Limits)
    edid[18] = 1;
    edid[19] = 3;

    // 4. Basic Display Parameters & Features (20..25)
    // 0x80 = Digital input (standard DVI/HDMI style, avoids DisplayPort AUX VRR probing)
    edid[20] = 0x80;
    edid[21] = ((width as u32 * 254) / 10000).clamp(10, 80) as u8;
    edid[22] = ((height as u32 * 254) / 10000).clamp(10, 80) as u8;
    edid[23] = 120; // Gamma 2.2
    // 0x02 = Preferred timing mode only (Bit 0 MUST be 0 so continuous frequency / VRR is disabled!)
    edid[24] = 0x02;

    // 5. Chromaticity Coordinates (Standard sRGB) (25..35)
    edid[25..35].copy_from_slice(&[
        0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54,
    ]);

    // 6. Established Timings (0x00 so only our exact native DTD resolution/orientation is exposed)
    edid[35] = 0x00;
    edid[36] = 0x00;
    edid[37] = 0x00;

    // 7. Standard Timings (38..54) - unused slots filled with 0x01, 0x01
    for i in (38..54).step_by(2) {
        edid[i] = 0x01;
        edid[i + 1] = 0x01;
    }

    // 8. Descriptor 1 (54..72): Detailed Timing Descriptor (DTD) for native resolution
    write_dtd(&mut edid[54..72], width, height, refresh_hz);

    // 9. Descriptor 2 (72..90): Dummy Descriptor (0x10) instead of Range Limits (0xFD)!
    // Using 0xFD with 30..144Hz caused `amdgpu` to enable FreeSync/VRR (`dm_handle_vmin_vmax_update`),
    // stalling the kernel workqueue for >10ms per frame and dropping refresh rate to 30Hz!
    edid[72..90].copy_from_slice(&[
        0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00,
    ]);

    // 10. Descriptor 3 (90..108): Monitor Name ("DroidMonitor")
    edid[90..108].copy_from_slice(&[
        0x00, 0x00, 0x00, 0xFC, 0x00, b'D', b'r', b'o', b'i', b'd', b'M', b'o', b'n', b'i',
        b't', b'o', b'r', 0x0A,
    ]);

    // 11. Descriptor 4 (108..126): Monitor Serial ("DM-USB-01")
    edid[108..126].copy_from_slice(&[
        0x00, 0x00, 0x00, 0xFF, 0x00, b'D', b'M', b'-', b'U', b'S', b'B', b'-', b'0', b'1',
        0x0A, 0x20, 0x20, 0x20,
    ]);

    // 12. Extension flag (0) & Checksum
    edid[126] = 0;
    let sum: u8 = edid[0..127].iter().fold(0u8, |acc, &x| acc.wrapping_add(x));
    edid[127] = 0u8.wrapping_sub(sum);

    edid
}

fn write_dtd(dtd: &mut [u8], width: u16, height: u16, refresh_hz: u16) {
    let h_active = width;
    let h_blank: u16 = 160;
    let h_front_porch: u16 = 48;
    let h_sync_width: u16 = 32;

    let v_active = height;
    let v_blank: u16 = ((height as u32 * 3) / 100).clamp(25, 90) as u16;
    let v_front_porch: u16 = 3;
    let v_sync_width: u16 = 6;

    let h_total = (h_active + h_blank) as u64;
    let v_total = (v_active + v_blank) as u64;
    let pixel_clock_10khz =
        (((h_total * v_total * refresh_hz as u64) + 5_000) / 10_000).clamp(100, 65535) as u16;

    dtd[0] = (pixel_clock_10khz & 0xFF) as u8;
    dtd[1] = (pixel_clock_10khz >> 8) as u8;

    dtd[2] = (h_active & 0xFF) as u8;
    dtd[3] = (h_blank & 0xFF) as u8;
    dtd[4] = (((h_active >> 8) & 0x0F) as u8) << 4 | (((h_blank >> 8) & 0x0F) as u8);

    dtd[5] = (v_active & 0xFF) as u8;
    dtd[6] = (v_blank & 0xFF) as u8;
    dtd[7] = (((v_active >> 8) & 0x0F) as u8) << 4 | (((v_blank >> 8) & 0x0F) as u8);

    dtd[8] = (h_front_porch & 0xFF) as u8;
    dtd[9] = (h_sync_width & 0xFF) as u8;
    dtd[10] = (((v_front_porch & 0x0F) as u8) << 4) | ((v_sync_width & 0x0F) as u8);
    dtd[11] = ((((h_front_porch >> 8) & 0x03) as u8) << 6)
        | ((((h_sync_width >> 8) & 0x03) as u8) << 4)
        | ((((v_front_porch >> 4) & 0x03) as u8) << 2)
        | (((v_sync_width >> 4) & 0x03) as u8);

    let width_mm = ((width as u32 * 254) / 1000).clamp(100, 1000) as u16;
    let height_mm = ((height as u32 * 254) / 1000).clamp(100, 1000) as u16;
    dtd[12] = (width_mm & 0xFF) as u8;
    dtd[13] = (height_mm & 0xFF) as u8;
    dtd[14] = (((width_mm >> 8) & 0x0F) as u8) << 4 | (((height_mm >> 8) & 0x0F) as u8);

    dtd[15] = 0;
    dtd[16] = 0;
    dtd[17] = 0x1E;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edid_checksum_and_header() {
        for &(w, h, hz) in &[(1920, 1080, 60), (2000, 1200, 60), (1080, 2400, 60), (1280, 720, 60)] {
            let edid = generate_edid(w, h, hz);
            assert_eq!(&edid[0..8], &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
            let sum: u8 = edid.iter().fold(0u8, |acc, &x| acc.wrapping_add(x));
            assert_eq!(sum, 0, "Invalid EDID checksum for {}x{}@{}", w, h, hz);
        }
    }
}
