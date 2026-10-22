//! Собственный VA-API H.264-энкодер (zero-copy): dmabuf-слот захвата →
//! VA-API импорт (P1) → VPP RGB→NV12 (P2) → EncSlice H.264 (P3) → Annex-B AU.
//! Вместо внешнего процесса ffmpeg; ffmpeg остаётся fallback-переключателем
//! NX_ENCODER=ffmpeg (дефолт). Порт проверенного C-пробника tools/va_p3_enc.c.
//!
//! Рецепт буферов (LIBVA_TRACE живого ffmpeg, /tmp/vafft.*):
//!   IDR: seq(1132) + misc RC(64) + HRD(28) + FrameRate(28) + picture(648)
//!        + packed SEQUENCE(SPS+PPS) + packed SLICE + slice(3140)   = 10 буферов
//!   P:   picture + packed SLICE + slice                            = 4 буфера
//!
//! Packed-заголовки SPS/PPS/slice парсятся Mesa в состояние контекста
//! (picture_h264_enc.c:603/754/438) → обязаны быть валидным H.264-синтаксисом
//! с эмуляционными байтами; slice-заголовок — первого слайса (first_mb=0).

use anyhow::{bail, Context, Result};
use std::os::fd::{AsRawFd, RawFd};

#[link(name = "va")]
extern "C" {
    fn vaInitialize(dpy: *mut std::os::raw::c_void, maj: *mut i32, min: *mut i32) -> i32;
    fn vaTerminate(dpy: *mut std::os::raw::c_void) -> i32;
    fn vaErrorStr(status: i32) -> *const std::os::raw::c_char;
    fn vaCreateConfig(
        dpy: *mut std::os::raw::c_void, profile: i32, entrypoint: i32,
        attribs: *const VaConfigAttrib, num_attribs: u32, config: *mut u32,
    ) -> i32;
    fn vaDestroyConfig(dpy: *mut std::os::raw::c_void, config: u32) -> i32;
    fn vaCreateSurfaces(
        dpy: *mut std::os::raw::c_void, format: u32, width: u32, height: u32,
        surfaces: *mut u32, num_surfaces: u32,
        attribs: *const VaSurfaceAttrib, num_attribs: u32,
    ) -> i32;
    fn vaDestroySurfaces(dpy: *mut std::os::raw::c_void, surfaces: *mut u32, num: u32) -> i32;
    fn vaCreateContext(
        dpy: *mut std::os::raw::c_void, config: u32, width: u32, height: u32,
        flags: u32, buffers: *mut u32, num_buffers: u32, context: *mut u32,
    ) -> i32;
    fn vaDestroyContext(dpy: *mut std::os::raw::c_void, context: u32) -> i32;
    fn vaCreateBuffer(
        dpy: *mut std::os::raw::c_void, context: u32, btype: i32, size: u32,
        num_elements: u32, data: *mut std::os::raw::c_void, buf: *mut u32,
    ) -> i32;
    fn vaMapBuffer(dpy: *mut std::os::raw::c_void, buf: u32, pbuf: *mut *mut std::os::raw::c_void) -> i32;
    fn vaUnmapBuffer(dpy: *mut std::os::raw::c_void, buf: u32) -> i32;
    fn vaDestroyBuffer(dpy: *mut std::os::raw::c_void, buf: u32) -> i32;
    fn vaBeginPicture(dpy: *mut std::os::raw::c_void, context: u32, surface: u32) -> i32;
    fn vaRenderPicture(dpy: *mut std::os::raw::c_void, context: u32, buffers: *mut u32, num: u32) -> i32;
    fn vaEndPicture(dpy: *mut std::os::raw::c_void, context: u32) -> i32;
    fn vaSyncSurface(dpy: *mut std::os::raw::c_void, surface: u32) -> i32;
    fn vaSyncBuffer(dpy: *mut std::os::raw::c_void, buf_id: u32, timeout_ns: u64) -> i32;
}

#[link(name = "va-drm")]
extern "C" {
    fn vaGetDisplayDRM(fd: std::os::raw::c_int) -> *mut std::os::raw::c_void;
}

// --- Константы libva (va.h / va_enc_h264.h / va_drmcommon.h) ---
const VA_STATUS_SUCCESS: i32 = 0x0000_0000;
const VA_INVALID_ID: u32 = 0xffff_ffff;
const VA_PROGRESSIVE: u32 = 0x0000_0001;

const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001; // va.h:1083 (0x10 — greyscale!)
const VA_RT_FORMAT_RGB32: u32 = 0x0002_0000;

const VA_PROFILE_NONE: i32 = -1; // va.h:504 (0 = MPEG2Simple!)
const VA_PROFILE_H264_HIGH: i32 = 7;
const VA_ENTRYPOINT_VIDEOPROC: i32 = 10;
const VA_ENTRYPOINT_ENCSLICE: i32 = 6;

const VA_RC_CBR: u32 = 0x0000_0002;
const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 0x0000_0001;
const VA_ENC_PACKED_HEADER_SLICE: u32 = 0x0000_0004;
const VA_ENC_PACKED_HEADER_MISC: u32 = 0x0000_0008;

const VA_FOURCC_BGRX: u32 = 0x5852_4742;
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u32 = 0x4000_0000;
const VA_SURFACE_ATTRIB_SETTABLE: u32 = 0x0000_0002;
const VASURFACE_ATTRIB_MEMORY_TYPE: u32 = 6;
const VASURFACE_ATTRIB_EXTERNAL_BUFFER_DESCRIPTOR: u32 = 7;
const VA_GENERIC_VALUE_TYPE_INTEGER: i32 = 1;
const VA_GENERIC_VALUE_TYPE_POINTER: i32 = 3;

const VABUFFER_TYPE_SEQ: i32 = 22;
const VABUFFER_TYPE_PIC: i32 = 23;
const VABUFFER_TYPE_SLICE: i32 = 24;
const VABUFFER_TYPE_PACKED_HDR_PARAM: i32 = 25;
const VABUFFER_TYPE_PACKED_HDR_DATA: i32 = 26;
const VABUFFER_TYPE_MISC: i32 = 27;
const VABUFFER_TYPE_CODED: i32 = 21;
const VA_PROC_PIPELINE_PARAM: i32 = 41;

const VA_ENC_PACKED_HEADER_SEQ_T: u32 = 1;
const VA_ENC_PACKED_HEADER_SLICE_T: u32 = 3;
const VA_ENC_MISC_TYPE_FRAMERATE: u32 = 0;
const VA_ENC_MISC_TYPE_RATECONTROL: u32 = 1;
const VA_ENC_MISC_TYPE_HRD: u32 = 5;

const VA_PICTURE_H264_INVALID: u32 = 0x0000_0001;
const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x0000_0008;

const VA_PROC_COLOR_STANDARD_BT601: i32 = 1; // va_vpp.h:307-319 (None=0, BT601=1)
const VA_PROC_COLOR_STANDARD_SRGB: i32 = 8;  // ...GenericFilm=7, SRGB=8

// --- repr(C) структуры; размеры/офсеты сверены с C offsetof (/tmp/va_off) ---

#[repr(C)]
#[derive(Clone, Copy)]
struct VaConfigAttrib {
    type_: u32,
    value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaGenericValueInteger {
    type_: i32,
    value: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaGenericValuePointer {
    type_: i32,
    _pad: i32,
    ptr: *mut std::os::raw::c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaSurfaceAttrib {
    type_: u32,
    flags: u32,
    value: VaGenericValue,
}

// VAGenericValue (va.h:1662): { type: i32, value: union { i32, float, void*, fn } }.
// union выравнен по 8 (указатель) → смещение value = 8, размер = 16.
#[repr(C)]
#[derive(Clone, Copy)]
struct VaGenericValue {
    type_: i32,
    _pad: i32,
    value: VaGenericValueUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
union VaGenericValueUnion {
    i: i32,
    f: f32,
    p: *mut std::os::raw::c_void,
    func: *mut std::os::raw::c_void,
}

impl VaGenericValue {
    fn integer(v: i32) -> Self {
        VaGenericValue {
            type_: VA_GENERIC_VALUE_TYPE_INTEGER,
            _pad: 0,
            value: VaGenericValueUnion { i: v },
        }
    }
    fn pointer(p: *mut std::os::raw::c_void) -> Self {
        VaGenericValue {
            type_: VA_GENERIC_VALUE_TYPE_POINTER,
            _pad: 0,
            value: VaGenericValueUnion { p },
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaDrmPrimeObject {
    fd: i32,
    size: u32,
    drm_format_modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaDrmPrimeLayer {
    drm_format: u32,
    num_planes: u32,
    object_index: [u32; 4],
    offset: [u32; 4],
    pitch: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaDrmPrimeSurfaceDescriptor {
    fourcc: u32,
    width: u32,
    height: u32,
    num_objects: u32,
    objects: [VaDrmPrimeObject; 4],
    num_layers: u32,
    layers: [VaDrmPrimeLayer; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VaPictureH264 {
    picture_id: u32,
    frame_idx: u32,
    flags: u32,
    top_field_order_cnt: i32,
    bottom_field_order_cnt: i32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaEncSequenceParameterBufferH264 {
    seq_parameter_set_id: u8,
    level_idc: u8,
    _pad0: u16,
    intra_period: u32,
    intra_idr_period: u32,
    ip_period: u32,
    bits_per_second: u32,
    max_num_ref_frames: u32,
    picture_width_in_mbs: u16,
    picture_height_in_mbs: u16,
    seq_fields: u32,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    num_ref_frames_in_pic_order_cnt_cycle: u8,
    _pad1: u8,
    offset_for_non_ref_pic: i32,
    offset_for_top_to_bottom_field: i32,
    offset_for_ref_frame: [i32; 256],
    frame_cropping_flag: u8,
    _pad2: [u8; 3],
    frame_crop_left_offset: u32,
    frame_crop_right_offset: u32,
    frame_crop_top_offset: u32,
    frame_crop_bottom_offset: u32,
    vui_parameters_present_flag: u8,
    _pad3: [u8; 3],
    vui_fields: u32,
    aspect_ratio_idc: u8,
    _pad4: [u8; 3],
    sar_width: u32,
    sar_height: u32,
    num_units_in_tick: u32,
    time_scale: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaEncPictureParameterBufferH264 {
    curr_pic: VaPictureH264,
    reference_frames: [VaPictureH264; 16],
    coded_buf: u32,
    pic_parameter_set_id: u8,
    seq_parameter_set_id: u8,
    _pad0: u16,
    frame_num: u16,
    pic_init_qp: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
    _pad1: u8,
    pic_fields: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaEncSliceParameterBufferH264 {
    macroblock_address: u32,
    num_macroblocks: u32,
    macroblock_info: u32,
    slice_type: u8,
    pic_parameter_set_id: u8,
    idr_pic_id: u16,
    pic_order_cnt_lsb: u16,
    _pad0: u16,
    delta_pic_order_cnt_bottom: i32,
    delta_pic_order_cnt: [i32; 2],
    direct_spatial_mv_pred_flag: u8,
    num_ref_idx_active_override_flag: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    ref_pic_list0: [VaPictureH264; 32],
    ref_pic_list1: [VaPictureH264; 32],
    luma_log2_weight_denom: u8,
    chroma_log2_weight_denom: u8,
    luma_weight_l0_flag: u8,
    _pad1: u8,
    luma_weight_l0: [i16; 32],
    luma_offset_l0: [i16; 32],
    chroma_weight_l0_flag: u8,
    _pad2: u8,
    chroma_weight_l0: [[i16; 2]; 32],
    chroma_offset_l0: [[i16; 2]; 32],
    luma_weight_l1_flag: u8,
    _pad3: u8,
    luma_weight_l1: [i16; 32],
    luma_offset_l1: [i16; 32],
    chroma_weight_l1_flag: u8,
    _pad4: u8,
    chroma_weight_l1: [[i16; 2]; 32],
    chroma_offset_l1: [[i16; 2]; 32],
    cabac_init_idc: u8,
    slice_qp_delta: i8,
    disable_deblocking_filter_idc: u8,
    slice_alpha_c0_offset_div2: i8,
    slice_beta_offset_div2: i8,
    _pad5: u8,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaEncPackedHeaderParameterBuffer {
    type_: u32,
    bit_length: u32,
    has_emulation_bytes: u8,
    _pad: [u8; 3],
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaEncMiscParameterRateControl {
    bits_per_second: u32,
    target_percentage: u32,
    window_size: u32,
    initial_qp: u32,
    min_qp: u32,
    max_qp: u32,
    rc_flags: u32,
    _pad: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaEncMiscParameterHRD {
    initial_buffer_fullness: u32,
    buffer_size: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaEncMiscParameterFrameRate {
    framerate: u32,
    flags: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaCodedBufferSegment {
    size: u32,
    bit_offset: u32,
    status: u32,
    _pad: u32,
    buf: *mut u8,
    next: *mut std::os::raw::c_void,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaProcPipelineParameterBuffer {
    surface: u32,
    _pad0: u32,
    surface_region: *mut std::os::raw::c_void,
    surface_color_standard: i32,
    _pad1: u32,
    output_region: *mut std::os::raw::c_void,
    output_background_color: u32,
    output_color_standard: i32,
    pipeline_flags: u32,
    filter_flags: u32,
    filters: *mut u32,
    num_filters: u32,
    _pad2: u32,
    forward_references: *mut u32,
    num_forward_references: u32,
    _pad3: u32,
    backward_references: *mut u32,
    num_backward_references: u32,
    rotation_state: u32,
    _pad4: u32,
    blend_state: *mut std::os::raw::c_void,
    mirror_state: u32,
    _pad5: u32,
    additional_outputs: *mut u32,
    num_additional_outputs: u32,
    input_surface_flag: u32,
    output_surface_flag: u32,
    _pad6: u32,
    input_color_properties: u32,
    output_color_properties: u32,
    processing_mode: u32,
    _pad7: u32,
    output_hdr_metadata: *mut std::os::raw::c_void,
    va_reserved: [u32; 16],
}

// Статическая сверка раскладки с C offsetof (/tmp/e39_va_off.txt, /tmp/sz).
const _: () = {
    assert!(std::mem::size_of::<VaEncSequenceParameterBufferH264>() == 1132);
    assert!(std::mem::size_of::<VaEncPictureParameterBufferH264>() == 648);
    assert!(std::mem::size_of::<VaEncSliceParameterBufferH264>() == 3140);
    assert!(std::mem::size_of::<VaEncPackedHeaderParameterBuffer>() == 28);
    assert!(std::mem::size_of::<VaCodedBufferSegment>() == 48);
    assert!(std::mem::size_of::<VaPictureH264>() == 36);
    assert!(std::mem::size_of::<VaDrmPrimeSurfaceDescriptor>() == 312);
    assert!(std::mem::size_of::<VaGenericValue>() == 16);
    assert!(std::mem::size_of::<VaSurfaceAttrib>() == 24);
    assert!(std::mem::offset_of!(VaEncSequenceParameterBufferH264, seq_fields) == 28);
    assert!(std::mem::offset_of!(VaEncSequenceParameterBufferH264, vui_fields) == 1092);
    assert!(std::mem::offset_of!(VaEncSequenceParameterBufferH264, sar_width) == 1100);
    assert!(std::mem::offset_of!(VaEncSequenceParameterBufferH264, num_units_in_tick) == 1108);
    assert!(std::mem::offset_of!(VaEncPictureParameterBufferH264, coded_buf) == 612);
    assert!(std::mem::offset_of!(VaEncPictureParameterBufferH264, pic_fields) == 628);
    assert!(std::mem::offset_of!(VaEncSliceParameterBufferH264, cabac_init_idc) == 3118);
    assert!(std::mem::offset_of!(VaCodedBufferSegment, buf) == 16);
};

// ---------- Бит-писатель RBSP + эмуляция + старт-коды (порт из C) ----------

struct BitWriter {
    data: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter { data: Vec::new(), acc: 0, nbits: 0 }
    }
    fn put(&mut self, val: u32, bits: u32) {
        for i in (0..bits).rev() {
            self.acc = (self.acc << 1) | ((val >> i) & 1);
            self.nbits += 1;
            if self.nbits == 8 {
                self.data.push(self.acc as u8);
                self.acc = 0;
                self.nbits = 0;
            }
        }
    }
    fn ue(&mut self, v: u32) {
        let t = v + 1;
        let lz = 31 - t.leading_zeros(); // floor(log2 t) = число ведущих нулей перед t
        self.put(0, lz);
        self.put(t, lz + 1);
    }
    fn se(&mut self, v: i32) {
        let u = if v <= 0 { (-2 * v as i64) as u32 } else { (2 * v - 1) as u32 };
        self.ue(u);
    }
    fn trailing(&mut self) {
        self.put(1, 1);
        while self.nbits != 0 {
            self.put(0, 1);
        }
    }
}

/// EBSP: вставка эмуляционных байтов 0x03 (два нуля подряд и байт ≤ 3).
fn ebsp_escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 16);
    let mut z = 0usize;
    for &b in rbsp {
        if z >= 2 && b <= 3 {
            out.push(0x03);
            z = 0;
        }
        out.push(b);
        z = if b == 0 { z + 1 } else { 0 };
    }
    out
}

/// NAL = 00 00 00 01 + forbidden/nri/type + EBSP(rbsp).
fn make_nal(nal_hdr: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + rbsp.len() + 16);
    out.extend_from_slice(&[0, 0, 0, 1, nal_hdr]);
    out.extend_from_slice(&ebsp_escape(rbsp));
    out
}

/// SPS (High, level 4.1, MBW×MBH MBs, POC type 2, VUI 60fps 1:1).
fn build_sps(mbw: u32, mbh: u32, gop: u32) -> Vec<u8> {
    let mut b = BitWriter::new();
    b.put(100, 8); // profile_idc = High
    b.put(0, 6); // constraint flags
    b.put(0, 2); // reserved
    b.put(41, 8); // level_idc = 4.1
    b.ue(0); // sps_id
    b.ue(1); // chroma_format_idc = 4:2:0
    b.ue(0); // bit_depth_luma_minus8
    b.ue(0); // bit_depth_chroma_minus8
    b.put(0, 1); // qpprime_y_zero_transform_bypass
    b.put(0, 1); // seq_scaling_matrix_present
    b.ue(4); // log2_max_frame_num_minus4
    b.ue(2); // pic_order_cnt_type = 2
    b.ue(1); // max_num_ref_frames
    b.put(0, 1); // gaps_in_frame_num
    b.ue(mbw - 1); // pic_width_in_mbs_minus1
    b.ue(mbh - 1); // pic_height_in_map_units_minus1
    b.put(1, 1); // frame_mbs_only_flag
    b.put(1, 1); // direct_8x8_inference
    b.put(0, 1); // frame_cropping
    b.put(1, 1); // vui_parameters_present
    b.put(1, 1); // aspect_ratio_info_present
    b.put(1, 8); // aspect_ratio_idc = 1:1
    b.put(0, 1); // overscan
    b.put(0, 1); // video_signal
    b.put(0, 1); // chroma_loc
    b.put(1, 1); // timing_info_present
    b.put(1, 32); // num_units_in_tick
    b.put(120, 32); // time_scale (fps задаётся сигнатурой; для 60 → 120)
    b.put(1, 1); // fixed_frame_rate
    b.put(0, 1); // nal_hrd
    b.put(0, 1); // vcl_hrd
    b.put(0, 1); // pic_struct_present
    b.put(1, 1); // bitstream_restriction
    b.put(1, 1); // motion_vectors_over_pic_boundaries
    b.ue(2); // max_bytes_per_pic_denom
    b.ue(1); // max_bits_per_mb_denom
    b.ue(15); // log2_max_mv_length_h
    b.ue(15); // log2_max_mv_length_v
    b.ue(0); // max_num_reorder_frames
    b.ue(1); // max_dec_frame_buffering
    let _ = gop;
    b.trailing();
    make_nal(0x67, &b.data) // nri=3, type=7
}

/// PPS (CABAC, High-tail transform_8x8=1).
fn build_pps() -> Vec<u8> {
    let mut b = BitWriter::new();
    b.ue(0); // pps_id
    b.ue(0); // sps_id
    b.put(1, 1); // entropy_coding_mode_flag = CABAC
    b.put(0, 1); // bottom_field_pic_order_in_frame_present
    b.ue(0); // num_slice_groups_minus1
    b.ue(0); // num_ref_idx_l0_default_active_minus1
    b.ue(0); // num_ref_idx_l1_default_active_minus1
    b.put(0, 1); // weighted_pred
    b.put(0, 2); // weighted_bipred_idc
    b.se(0); // pic_init_qp_minus26
    b.se(0); // pic_init_qs_minus26
    b.se(0); // chroma_qp_index_offset
    b.put(0, 1); // deblocking_filter_control_present
    b.put(0, 1); // constrained_intra_pred
    b.put(0, 1); // redundant_pic_cnt_present
    b.put(1, 1); // transform_8x8_mode (High tail)
    b.put(0, 1); // pic_scaling_matrix_present
    b.se(0); // second_chroma_qp_index_offset
    b.trailing();
    make_nal(0x68, &b.data) // nri=3, type=8
}

/// Slice-заголовок первого слайса картинки (first_mb=0 → Mesa читает до конца).
fn build_slice_hdr(idr: bool, frame_num: u32) -> Vec<u8> {
    let mut b = BitWriter::new();
    b.ue(0); // first_mb_in_slice
    b.ue(if idr { 2 } else { 0 }); // slice_type: I=2 (IDR), P=0
    b.ue(0); // pic_parameter_set_id
    b.put(frame_num, 8); // frame_num (log2_max_frame_num = 8)
    if idr {
        b.ue(0); // idr_pic_id
        b.put(0, 1); // no_output_of_prior_pics_flag
        b.put(0, 1); // long_term_reference_flag
    } else {
        b.put(0, 1); // num_ref_idx_active_override_flag
        b.put(0, 1); // ref_pic_list_modification_flag_l0
        b.put(0, 1); // adaptive_ref_pic_marking_mode_flag
        b.ue(0); // cabac_init_idc (CABAC && P)
    }
    b.se(0); // slice_qp_delta
    b.trailing();
    let nal = if idr { 0x65 } else { 0x41 }; // nri=3/type=5 | nri=2/type=1
    make_nal(nal, &b.data)
}

/// Хелпер: проверить VA-статус.
fn va_ok(st: i32, ctx: &str) -> Result<()> {
    if st == VA_STATUS_SUCCESS {
        return Ok(());
    }
    let s = unsafe {
        let p = vaErrorStr(st);
        if p.is_null() {
            format!("status {}", st)
        } else {
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    bail!("{}: {}", ctx, s)
}

/// RAII-очистка VA-объектов (деструктор вызывает vaTerminate).
struct VaCtx {
    dpy: *mut std::os::raw::c_void,
}

impl Drop for VaCtx {
    fn drop(&mut self) {
        if !self.dpy.is_null() {
            unsafe { vaTerminate(self.dpy) };
        }
    }
}

/// Собственный VA-API кодировщик: dmabuf-fd (BGRX) → VPP NV12 → H.264 Annex-B.
// Send: FFI-указатели не тикают ленивые данные; энкодер используется из
// одного потока (writer→encoder в pipeline.rs), Send нужен для Arc<Mutex<..>>.
unsafe impl Send for VaEncoder {}

pub struct VaEncoder {
    va: VaCtx,
    /// fd /dev/dri/renderD128 — держим открытым на весь срок (VA может ходить по нему).
    _render_fd: std::fs::File,
    /// Размер источника (захвата).
    w: u32,
    h: u32,
    /// Размер выхода (энкодера; = w,h либо масштаб VPP).
    ow: u32,
    oh: u32,
    mbw: u32,
    mbh: u32,
    bitrate: u32,
    gop: u32,
    fps: u32,
    vctx: u32,
    ectx: u32,
    recon: [u32; 2],
    /// Импортированные BGRX-поверхности обоих capture-слотов (источники VPP).
    srcs: [u32; 2],
    /// NV12-поверхности VPP (ping-pong). Кадр N кодируется из vdst[N%2];
    /// кадр N-1 ещё читает vdst[(N-1)%2] — одна общая поверхность заставляла
    /// VPP ждать освобождения от энкодера (замер: p95 69 мс при p50 6.9).
    vdst: [u32; 2],
    coded_size: u32,
    frame_idx: u32,
    /// frame_num в битстриме (H.264 7.4.3): в IDR обязан быть 0, дальше
    /// растёт на 1 до следующего IDR. Раньше писался глобальный счётчик
    /// frame_idx (40, 80, …) → декодер отвергал все ключевые кадры после
    /// первого и клиент вечно ждал "Waiting for IDR frame".
    frame_num: u32,
    /// Форсировать IDR на следующем кадре (запрос клиента control 0x0302).
    pub force_idr: bool,
    /// Coded-буферы на кадр (пул из 2: текущий + предыдущий в полёте).
    /// Раньше vaCreateBuffer на 2.67 МБ создавался/уничтожался каждый кадр —
    /// при 60 fps это 160 МБ/с аллокаций и страничных отказов.
    coded: [u32; 2],
    /// Последний замер (vpp, setup, submit, sync) в мс — NX_VAAPI_TIMING.
    pub timing: (f64, f64, f64, f64),
    /// Раздельные замеры ожидания: fence поверхности/буфера и чтения (map).
    pub last_surf_ms: f64,
    pub last_map_ms: f64,
    /// Накопители для средних/максимумов за окно лога (сбрасываются отчётом).
    tim_sum: (f64, f64, f64, f64),
    tim_max: (f64, f64, f64, f64),
    tim_n: u64,
    /// Кадр в полёте: (coded_buf, created_bufs). Sync/читается на СЛЕДУЮЩЕМ кадре.
    prev: Option<(u32, Vec<u32>)>,
}

impl VaEncoder {
    /// Создание VPP/энкодер-контекстов (без поверхностей-источников).
    /// dmabuf-слоты импортируются отдельно через import_surfaces (поверхности
    /// живут ровно столько, сколько живут слот-буферы захвата).
    pub fn new(w: u32, h: u32, ow: u32, oh: u32, bitrate: u32, gop: u32, fps: u32) -> Result<Self> {
        let render = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/dri/renderD128")
            .context("открыть /dev/dri/renderD128")?;
        let dpy = unsafe { vaGetDisplayDRM(render.as_raw_fd()) };
        if dpy.is_null() {
            bail!("vaGetDisplayDRM вернул NULL");
        }
        let (mut maj, mut min) = (0i32, 0i32);
        unsafe { va_ok(vaInitialize(dpy, &mut maj, &mut min), "vaInitialize")? };
        println!("vaapi: libva {}.{}", maj, min);
        let va = VaCtx { dpy };

        let mbw = (ow + 15) / 16;
        let mbh = (oh + 15) / 16;
        let coded_size = (3 * ow * oh + 65536) as u32;

        // NV12-поверхности VPP (выходного размера — VPP делает И конверсию, И
        // скейл за один проход: surface→dst разными размерами) + VPP-контекст.
        // Две поверхности: пока VCE читает vdst[N%2], VPP уже пишет vdst[(N+1)%2].
        let mut vdst = [VA_INVALID_ID; 2];
        unsafe {
            va_ok(
                vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, ow, oh, vdst.as_mut_ptr(), 2, std::ptr::null(), 0),
                "vaCreateSurfaces(vpp dst)",
            )?;
        }
        let mut vpp_cfg = VA_INVALID_ID;
        unsafe {
            va_ok(
                vaCreateConfig(
                    dpy,
                    VA_PROFILE_NONE,
                    VA_ENTRYPOINT_VIDEOPROC,
                    std::ptr::null(),
                    0,
                    &mut vpp_cfg,
                ),
                "vaCreateConfig(vpp)",
            )?;
        }
        let mut vctx = VA_INVALID_ID;
        unsafe {
            va_ok(
                vaCreateContext(dpy, vpp_cfg, ow, oh, 0x1, std::ptr::null_mut(), 0, &mut vctx),
                "vaCreateContext(vpp)",
            )?;
        }

        // Энкодер-конфиг: H264High / EncSlice / CBR / PackedHeaders=SEQ|SLICE|MISC (P3).
        let ecattr = [
            VaConfigAttrib { type_: 0, value: VA_RT_FORMAT_YUV420 },          // RTFormat
            VaConfigAttrib { type_: 5, value: VA_RC_CBR },                     // RateControl
            VaConfigAttrib {
                type_: 10,
                value: VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_SLICE
                    | VA_ENC_PACKED_HEADER_MISC,
            }, // EncPackedHeaders
        ];
        let mut enc_cfg = VA_INVALID_ID;
        unsafe {
            va_ok(
                vaCreateConfig(
                    dpy,
                    VA_PROFILE_H264_HIGH,
                    VA_ENTRYPOINT_ENCSLICE,
                    ecattr.as_ptr(),
                    3,
                    &mut enc_cfg,
                ),
                "vaCreateConfig(enc)",
            )?;
        }
        let mut ectx = VA_INVALID_ID;
        unsafe {
            va_ok(
                vaCreateContext(dpy, enc_cfg, ow, oh, VA_PROGRESSIVE, std::ptr::null_mut(), 0, &mut ectx),
                "vaCreateContext(enc)",
            )?;
        }

        // Пул coded-буферов: текущий кадр + предыдущий в полёте.
        let mut coded = [VA_INVALID_ID; 2];
        for c in coded.iter_mut() {
            unsafe {
                va_ok(
                    vaCreateBuffer(
                        dpy,
                        ectx,
                        VABUFFER_TYPE_CODED,
                        coded_size,
                        1,
                        std::ptr::null_mut(),
                        c,
                    ),
                    "vaCreateBuffer(coded pool)",
                )?;
            }
        }

        // Recon DPB: пинг-pong (текущая + предыдущая = референс).
        let mut recon = [VA_INVALID_ID; 2];
        unsafe {
            va_ok(
                vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, ow, oh, recon.as_mut_ptr(), 2, std::ptr::null(), 0),
                "vaCreateSurfaces(recon)",
            )?;
        }

        // FPS в misc-буферах (сообщается кодеку для CBR) — см. encode_frame.

        Ok(VaEncoder {
            va,
            _render_fd: render,
            w,
            h,
            ow,
            oh,
            mbw,
            mbh,
            bitrate,
            gop,
            fps,
            vctx,
            ectx,
            recon,
            srcs: [VA_INVALID_ID; 2],
            vdst,
            coded,
            coded_size,
            frame_idx: 0,
            frame_num: 0,
            force_idr: false,
            timing: (0.0, 0.0, 0.0, 0.0),
            last_surf_ms: 0.0,
            last_map_ms: 0.0,
            tim_sum: (0.0, 0.0, 0.0, 0.0),
            tim_max: (0.0, 0.0, 0.0, 0.0),
            tim_n: 0,
            prev: None,
        })
    }

    /// Импорт dmabuf-fd capture-слота в VA-поверхность (P1). slot ∈ {0,1}.
    /// Поверхность ссылается на fd, а не владеет им: fd обязан жить, пока
    /// импортирована (слот-буфер держит). Переповторный вызов по тому же слоту
    /// пересоздаёт поверхность (после пересоздания dumb-буфера при смене
    /// геометрии).
    pub fn import_slot(&mut self, slot: usize, dmabuf: RawFd, pitch: u32) -> Result<()> {
        anyhow::ensure!(slot < 2, "slot {} вне 0..2", slot);
        let dpy = self.va.dpy;
        let (w, h) = (self.w, self.h);
        let sz = pitch as usize * h as usize;

        let mut desc = VaDrmPrimeSurfaceDescriptor {
            fourcc: VA_FOURCC_BGRX,
            width: w,
            height: h,
            num_objects: 1,
            objects: [VaDrmPrimeObject { fd: 0, size: 0, drm_format_modifier: 0 }; 4],
            num_layers: 1,
            layers: [VaDrmPrimeLayer {
                drm_format: DRM_FORMAT_XRGB8888,
                num_planes: 1,
                object_index: [0; 4],
                offset: [0; 4],
                pitch: [0; 4],
            }; 4],
        };
        desc.objects[0].fd = dmabuf;
        desc.objects[0].size = sz as u32;
        desc.layers[0].pitch[0] = pitch;

        let attrs = [
            VaSurfaceAttrib {
                type_: VASURFACE_ATTRIB_MEMORY_TYPE,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VaGenericValue::integer(VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i32),
            },
            VaSurfaceAttrib {
                type_: VASURFACE_ATTRIB_EXTERNAL_BUFFER_DESCRIPTOR,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VaGenericValue::pointer(&mut desc as *mut _ as *mut _),
            },
        ];
        let mut src = VA_INVALID_ID;
        unsafe {
            va_ok(
                vaCreateSurfaces(
                    dpy,
                    VA_RT_FORMAT_RGB32,
                    w,
                    h,
                    &mut src,
                    1,
                    attrs.as_ptr(),
                    2,
                ),
                "vaCreateSurfaces(import slot)",
            )?;
        }
        if self.srcs[slot] != VA_INVALID_ID {
            unsafe { vaDestroySurfaces(dpy, &mut self.srcs[slot], 1) };
        }
        self.srcs[slot] = src;
        Ok(())
    }

    /// Импортирован ли уже слот (для ленивой инициализации второго слота).
    pub fn slot_ready(&self, slot: usize) -> bool {
        slot < 2 && self.srcs[slot] != VA_INVALID_ID
    }

    /// Освободить импорт слота (при drop_slot_buf без смерти энкодера).
    pub fn release_slot(&mut self, slot: usize) {
        if slot < 2 && self.srcs[slot] != VA_INVALID_ID {
            unsafe { vaDestroySurfaces(self.va.dpy, &mut self.srcs[slot], 1) };
            self.srcs[slot] = VA_INVALID_ID;
        }
    }

    /// VPP-стадия: RGB(BGRX) → NV12 (та же, что в PASS-пробнике va_p2_vpp.c).
    /// Пайплайн: surface=src → filters (csc sRGB→BT.601) → output=vdst.
    /// NX_VAAPI_TIMING=1 → копит время sync-а VPP (GPU-конвея) отдельно.
    fn run_vpp(&mut self, src: u32, vdst: u32) -> Result<()> {
        // Цветовые стандарты: источник sRGB, выход BT.601 (P2-решение).
        let mut pp: VaProcPipelineParameterBuffer = unsafe { std::mem::zeroed() };
        pp.surface = src;
        pp.surface_color_standard = VA_PROC_COLOR_STANDARD_SRGB;
        pp.output_color_standard = VA_PROC_COLOR_STANDARD_BT601;

        let mut bids: Vec<u32> = Vec::new();
        let mut created: Vec<u32> = Vec::new();
        let mut b = VA_INVALID_ID;
        unsafe {
            va_ok(
                vaCreateBuffer(
                    self.va.dpy,
                    self.vctx,
                    VA_PROC_PIPELINE_PARAM,
                    std::mem::size_of::<VaProcPipelineParameterBuffer>() as u32,
                    1,
                    &mut pp as *mut _ as *mut _,
                    &mut b,
                ),
                "vaCreateBuffer(vpp)",
            )?;
        }
        bids.push(b);
        created.push(b);

        unsafe {
            va_ok(vaBeginPicture(self.va.dpy, self.vctx, vdst), "vaBeginPicture(vpp)")?;
            let r = vaRenderPicture(self.va.dpy, self.vctx, bids.as_mut_ptr(), bids.len() as u32);
            if r != VA_STATUS_SUCCESS {
                self.cleanup(&created);
                va_ok(r, "vaRenderPicture(vpp)")?;
            }
            if let Err(e) = va_ok(vaEndPicture(self.va.dpy, self.vctx), "vaEndPicture(vpp)") {
                self.cleanup(&created);
                return Err(e);
            }
        }
        self.cleanup(&created);
        Ok(())
    }

    /// Кодирует один кадр из dmabuf-слота `slot` (поверхность уже импортирована
    /// через import_slot). Возвращает Annex-B AU (SPS+PPS+IDR на ключевом,
    /// slice — на P-кадре).
    pub fn encode_slot(&mut self, slot: usize) -> Result<Vec<u8>> {
        let t_total = std::time::Instant::now();
        let i = self.frame_idx;
        // IDR — по GOP или по запросу клиента (control 0x0302).
        let idr = self.force_idr || i % self.gop == 0;
        self.force_idr = false;
        // frame_num в битстриме: в IDR = 0, дальше +1 до следующего IDR
        // (H.264 7.4.3). Глобальный счётчик i здесь ломал цепочку ссылок:
        // после первого IDR декодер отвергал все ключевые кадры.
        let fnum = if idr { 0 } else { self.frame_num.wrapping_add(1) };
        self.frame_num = fnum;

        // Источник → NV12 (vdst). Поверхность чередуется по кадрам: кадр N-1
        // ещё может кодироваться из своей vdst, пока VPP готовит следующую.
        let vdst = self.vdst[(i % 2) as usize];
        self.run_vpp(self.srcs[slot], vdst)?;
        let t_vpp = t_total.elapsed();

        let mut bids: Vec<u32> = Vec::with_capacity(12);
        let mut created: Vec<u32> = Vec::with_capacity(13);
        let mut mk = |dpy: *mut std::os::raw::c_void,
                      ectx: u32,
                      btype: i32,
                      data: *mut std::os::raw::c_void,
                      size: u32|
         -> Result<u32> {
            let mut b = VA_INVALID_ID;
            unsafe { va_ok(vaCreateBuffer(dpy, ectx, btype, size, 1, data, &mut b), "vaCreateBuffer")? };
            Ok(b)
        };

        if idr {
            // sequence (только IDR)
            let mut seq: VaEncSequenceParameterBufferH264 = unsafe { std::mem::zeroed() };
            seq.level_idc = 41;
            seq.intra_period = self.gop;
            seq.intra_idr_period = self.gop;
            seq.ip_period = 1;
            seq.bits_per_second = self.bitrate;
            seq.max_num_ref_frames = 1;
            seq.picture_width_in_mbs = self.mbw as u16;
            seq.picture_height_in_mbs = self.mbh as u16;
            // seq_fields: chroma_format_idc=1, frame_mbs_only=1, direct_8x8=1,
            // log2_max_frame_num_minus4=4, pic_order_cnt_type=2.
            seq.seq_fields = 1 | (1 << 2) | (1 << 5) | (4 << 6) | (2 << 10);
            seq.vui_parameters_present_flag = 1;
            // vui_fields (va_enc_h264.h:238-256): aspect_ratio@0, timing@1,
            // bitstream_restriction@2, log2_max_mv_h@3..7, log2_max_mv_v@8..12,
            // fixed_frame_rate@13.
            seq.vui_fields = 1 | (1 << 1) | (1 << 2) | (1 << 13) | (15 << 3) | (15 << 8);
            seq.aspect_ratio_idc = 1;
            seq.num_units_in_tick = 1;
            seq.time_scale = self.fps * 2; // num_units_in_tick=1 → fps fps
            let b = mk(
                self.va.dpy,
                self.ectx,
                VABUFFER_TYPE_SEQ,
                &mut seq as *mut _ as *mut _,
                std::mem::size_of::<VaEncSequenceParameterBufferH264>() as u32,
            )?;
            bids.push(b);
            created.push(b);
        }

        // RC/HRD/FrameRate — на КАЖДЫЙ кадр, не только на IDR. Раньше они
        // жили внутри `if idr`: при GOP=30 окно RC (500 мс = 30 кадров)
        // сбрасывалось IDR'ом, а при длинном GOP после ~30 P-кадров VCE
        // уходил в патологический режим (sync 170-250 мс/кадр). Замер:
        // GOP=40 → p95 15 мс, GOP=240 → p50 84 мс.
        {
            // misc RC
            let mut rcmem = [0u8; 4 + std::mem::size_of::<VaEncMiscParameterRateControl>()];
            rcmem[0..4].copy_from_slice(&(VA_ENC_MISC_TYPE_RATECONTROL as u32).to_le_bytes());
            let rc = unsafe { &mut *(&mut rcmem[4] as *mut u8 as *mut VaEncMiscParameterRateControl) };
            rc.bits_per_second = self.bitrate;
            rc.target_percentage = 100;
            rc.window_size = std::env::var("NX_RC_WINDOW")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(500);
            rc.initial_qp = 26;
            rc.min_qp = 10;
            rc.max_qp = 51;
            let b = mk(self.va.dpy, self.ectx, VABUFFER_TYPE_MISC, rcmem.as_mut_ptr() as *mut _, rcmem.len() as u32)?;
            bids.push(b);
            created.push(b);

            // misc HRD
            let mut hrdmem = [0u8; 4 + std::mem::size_of::<VaEncMiscParameterHRD>()];
            hrdmem[0..4].copy_from_slice(&(VA_ENC_MISC_TYPE_HRD as u32).to_le_bytes());
            let hrd = unsafe { &mut *(&mut hrdmem[4] as *mut u8 as *mut VaEncMiscParameterHRD) };
            hrd.buffer_size = self.bitrate;
            hrd.initial_buffer_fullness = self.bitrate / 2;
            let b = mk(self.va.dpy, self.ectx, VABUFFER_TYPE_MISC, hrdmem.as_mut_ptr() as *mut _, hrdmem.len() as u32)?;
            bids.push(b);
            created.push(b);

            // misc FrameRate
            let mut frmem = [0u8; 4 + std::mem::size_of::<VaEncMiscParameterFrameRate>()];
            frmem[0..4].copy_from_slice(&(VA_ENC_MISC_TYPE_FRAMERATE as u32).to_le_bytes());
            let fr = unsafe { &mut *(&mut frmem[4] as *mut u8 as *mut VaEncMiscParameterFrameRate) };
            fr.framerate = (1 << 16) | self.fps; // num=fps, den=1
            let b = mk(self.va.dpy, self.ectx, VABUFFER_TYPE_MISC, frmem.as_mut_ptr() as *mut _, frmem.len() as u32)?;
            bids.push(b);
            created.push(b);
        }

        // coded-буфер из пула: 2 буфера, текущий + предыдущий в полёте.
        let coded = self.coded[(i % 2) as usize];

        // picture
        let mut pic: VaEncPictureParameterBufferH264 = unsafe { std::mem::zeroed() };
        let cur = self.recon[(i % 2) as usize];
        pic.curr_pic.picture_id = cur;
        pic.curr_pic.frame_idx = fnum;
        pic.curr_pic.top_field_order_cnt = 2 * fnum as i32;
        pic.curr_pic.bottom_field_order_cnt = 2 * fnum as i32;
        for k in 0..16 {
            pic.reference_frames[k].picture_id = VA_INVALID_ID;
            pic.reference_frames[k].flags = VA_PICTURE_H264_INVALID;
        }
        if !idr {
            let prev = self.recon[((i + 1) % 2) as usize];
            pic.reference_frames[0].picture_id = prev;
            pic.reference_frames[0].frame_idx = fnum - 1;
            pic.reference_frames[0].flags = VA_PICTURE_H264_SHORT_TERM_REFERENCE;
            pic.reference_frames[0].top_field_order_cnt = 2 * (fnum as i32 - 1);
            pic.reference_frames[0].bottom_field_order_cnt = 2 * (fnum as i32 - 1);
        }
        pic.coded_buf = coded;
        pic.frame_num = fnum as u16;
        pic.pic_init_qp = 26;
        // pic_fields: idr_pic_flag, reference_pic_flag, entropy_coding_mode(CABAC),
        // transform_8x8_mode(High).
        pic.pic_fields = (if idr { 1 } else { 0 }) | (1 << 1) | (1 << 3) | (1 << 8);
        let b = mk(
            self.va.dpy,
            self.ectx,
            VABUFFER_TYPE_PIC,
            &mut pic as *mut _ as *mut _,
            std::mem::size_of::<VaEncPictureParameterBufferH264>() as u32,
        )?;
        bids.push(b);
        created.push(b);

        // packed SEQUENCE (SPS+PPS) — только IDR
        if idr {
            let mut nal = Vec::new();
            nal.extend_from_slice(&build_sps(self.mbw, self.mbh, self.gop));
            nal.extend_from_slice(&build_pps());
            let mut ph: VaEncPackedHeaderParameterBuffer = unsafe { std::mem::zeroed() };
            ph.type_ = VA_ENC_PACKED_HEADER_SEQ_T;
            ph.bit_length = (nal.len() * 8) as u32;
            ph.has_emulation_bytes = 1;
            let b = mk(
                self.va.dpy,
                self.ectx,
                VABUFFER_TYPE_PACKED_HDR_PARAM,
                &mut ph as *mut _ as *mut _,
                std::mem::size_of::<VaEncPackedHeaderParameterBuffer>() as u32,
            )?;
            bids.push(b);
            created.push(b);
            let b = mk(
                self.va.dpy,
                self.ectx,
                VABUFFER_TYPE_PACKED_HDR_DATA,
                nal.as_mut_ptr() as *mut _,
                nal.len() as u32,
            )?;
            bids.push(b);
            created.push(b);
        }

        // packed SLICE (каждый кадр)
        {
            let mut nal = build_slice_hdr(idr, fnum);
            let mut ph: VaEncPackedHeaderParameterBuffer = unsafe { std::mem::zeroed() };
            ph.type_ = VA_ENC_PACKED_HEADER_SLICE_T;
            ph.bit_length = (nal.len() * 8) as u32;
            ph.has_emulation_bytes = 1;
            let b = mk(
                self.va.dpy,
                self.ectx,
                VABUFFER_TYPE_PACKED_HDR_PARAM,
                &mut ph as *mut _ as *mut _,
                std::mem::size_of::<VaEncPackedHeaderParameterBuffer>() as u32,
            )?;
            bids.push(b);
            created.push(b);
            let b = mk(
                self.va.dpy,
                self.ectx,
                VABUFFER_TYPE_PACKED_HDR_DATA,
                nal.as_mut_ptr() as *mut _,
                nal.len() as u32,
            )?;
            bids.push(b);
            created.push(b);
        }

        // slice params
        let mut slc: VaEncSliceParameterBufferH264 = unsafe { std::mem::zeroed() };
        slc.num_macroblocks = self.mbw * self.mbh;
        slc.slice_type = if idr { 2 } else { 0 }; // I=2, P=0
        slc.pic_order_cnt_lsb = (2 * fnum) as u16;
        slc.direct_spatial_mv_pred_flag = 1;
        for k in 0..32 {
            slc.ref_pic_list0[k].picture_id = VA_INVALID_ID;
            slc.ref_pic_list0[k].flags = VA_PICTURE_H264_INVALID;
            slc.ref_pic_list1[k].picture_id = VA_INVALID_ID;
            slc.ref_pic_list1[k].flags = VA_PICTURE_H264_INVALID;
        }
        if !idr {
            slc.ref_pic_list0[0].picture_id = self.recon[((i + 1) % 2) as usize];
            slc.ref_pic_list0[0].frame_idx = fnum - 1;
            slc.ref_pic_list0[0].flags = VA_PICTURE_H264_SHORT_TERM_REFERENCE;
            slc.ref_pic_list0[0].top_field_order_cnt = 2 * (fnum as i32 - 1);
            slc.ref_pic_list0[0].bottom_field_order_cnt = 2 * (fnum as i32 - 1);
        }
        let b = mk(
            self.va.dpy,
            self.ectx,
            VABUFFER_TYPE_SLICE,
            &mut slc as *mut _ as *mut _,
            std::mem::size_of::<VaEncSliceParameterBufferH264>() as u32,
        )?;
        bids.push(b);
        created.push(b);

        // кодирование
        let t_setup = t_total.elapsed() - t_vpp;
        unsafe {
            va_ok(vaBeginPicture(self.va.dpy, self.ectx, vdst), "vaBeginPicture(enc)")?;
            let r = vaRenderPicture(self.va.dpy, self.ectx, bids.as_mut_ptr(), bids.len() as u32);
            if r != VA_STATUS_SUCCESS {
                self.cleanup(&created);
                va_ok(r, "vaRenderPicture(enc)")?;
            }
            if let Err(e) = va_ok(vaEndPicture(self.va.dpy, self.ectx), "vaEndPicture(enc)") {
                self.cleanup(&created);
                return Err(e);
            }
            let t_submit = t_total.elapsed() - t_vpp - t_setup;

            // Конвейер: кадр N засабмичен, НЕ ждём его. Sync+читаем кадр N-1
            // (его coded-буфер лежит в prev). VCE кодирует N, пока CPU
            // отдаёт N-1 в RTP — глубина конвейера 2 (как у ffmpeg async).
            let out = match self.prev.take() {
                Some((prev_coded, prev_created)) => {
                    let t0 = std::time::Instant::now();
                    let r = self.sync_read_coded(prev_coded);
                    let t_sync = t0.elapsed().as_secs_f64() * 1000.0;
                    let t_clean = std::time::Instant::now();
                    self.cleanup(&prev_created);
                    let clean_ms = t_clean.elapsed().as_secs_f64() * 1000.0;
                    if clean_ms > 5.0 {
                        println!("vaapi: cleanup {} буферов занял {:.1} мс", prev_created.len(), clean_ms);
                    }
                    self.timing = (t_vpp.as_secs_f64()*1000.0, t_setup.as_secs_f64()*1000.0, t_submit.as_secs_f64()*1000.0, t_sync);
                    self.acc_timing();
                    match r {
                        Ok(au) => au,
                        Err(e) => {
                            self.cleanup(&created);
                            return Err(e);
                        }
                    }
                }
                None => {
                    // Первый кадр: НЕ ждём его. Кладём в prev и возвращаем пустой
                    // AU — кадр 0 уйдёт на следующем вызове, когда засабмичен
                    // кадр 1. Иначе prev навсегда остаётся None и весь стрим
                    // работает в глубину 1 (каждый кадр ждётся synс'ом).
                    self.timing = (
                        t_vpp.as_secs_f64() * 1000.0,
                        t_setup.as_secs_f64() * 1000.0,
                        t_submit.as_secs_f64() * 1000.0,
                        0.0,
                    );
                    self.acc_timing();
                    self.prev = Some((coded, created));
                    self.frame_idx += 1;
                    return Ok(Vec::new());
                }
            };

            self.prev = Some((coded, created));
            self.frame_idx += 1;
            if std::env::var_os("NX_ENC_TRACE").is_some() {
                let total_ms = t_total.elapsed().as_secs_f64() * 1000.0;
                let (v, s, su, sy) = self.timing;
                println!("trace enc[{i}] total={total_ms:.1} vpp={v:.1} setup={s:.1} submit={su:.1} sync={sy:.1} surf={:.1} map={:.1}", self.last_surf_ms, self.last_map_ms);
            }
            Ok(out)
        }
    }

    /// Средние/максимумы стадий за окно лога; сбрасывает накопители.
    pub fn report_timing(&mut self) -> ((f64, f64, f64, f64), (f64, f64, f64, f64), u64) {
        let n = self.tim_n;
        let k = n.max(1) as f64;
        let avg = (
            self.tim_sum.0 / k,
            self.tim_sum.1 / k,
            self.tim_sum.2 / k,
            self.tim_sum.3 / k,
        );
        let max = self.tim_max;
        self.tim_sum = (0.0, 0.0, 0.0, 0.0);
        self.tim_max = (0.0, 0.0, 0.0, 0.0);
        self.tim_n = 0;
        (avg, max, n)
    }

    fn acc_timing(&mut self) {
        let (a, b, c, d) = self.timing;
        self.tim_sum.0 += a;
        self.tim_sum.1 += b;
        self.tim_sum.2 += c;
        self.tim_sum.3 += d;
        self.tim_max.0 = self.tim_max.0.max(a);
        self.tim_max.1 = self.tim_max.1.max(b);
        self.tim_max.2 = self.tim_max.2.max(c);
        self.tim_max.3 = self.tim_max.3.max(d);
        self.tim_n += 1;
    }

    /// Ждём готовности coded-буфера и читаем Annex-B AU.
    /// ВАЖНО: ожидания 170-650 мс на кадр при GOP=120/240 — патология драйвера
    /// VCE, а не vaSyncBuffer: при GOP=40 p95 sync ~10 мс, ffmpeg на том же
    /// VCE кодирует 1088x800@60 со 107 fps. Не поднимать GOP выше 60.
    fn sync_read_coded(&mut self, coded: u32) -> Result<Vec<u8>> {
        let t0 = std::time::Instant::now();
        unsafe { va_ok(vaSyncBuffer(self.va.dpy, coded, 10_000_000_000), "vaSyncBuffer")? };
        self.last_surf_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = std::time::Instant::now();
        let au = self.read_coded(coded)?;
        self.last_map_ms = t1.elapsed().as_secs_f64() * 1000.0;
        Ok(au)
    }

    fn read_coded(&mut self, coded: u32) -> Result<Vec<u8>> {
        let mut seg: *mut std::os::raw::c_void = std::ptr::null_mut();
        let mut au = Vec::new();
        unsafe {
            va_ok(vaMapBuffer(self.va.dpy, coded, &mut seg), "vaMapBuffer(coded)")?;
            let mut cur_seg = seg as *mut VaCodedBufferSegment;
            while !cur_seg.is_null() {
                let s = &*cur_seg;
                if s.size > 0 && !s.buf.is_null() {
                    let sl = std::slice::from_raw_parts(s.buf, s.size as usize);
                    au.extend_from_slice(sl);
                }
                cur_seg = s.next as *mut VaCodedBufferSegment;
            }
            vaUnmapBuffer(self.va.dpy, coded);
        }
        Ok(au)
    }

    /// Импортированная BGRX-поверхность (для записи кадра снаружи через mmap dmabuf).
    fn cleanup(&self, ids: &[u32]) {
        for &b in ids {
            unsafe { vaDestroyBuffer(self.va.dpy, b) };
        }
    }
}

impl Drop for VaEncoder {
    fn drop(&mut self) {
        // Незасинхроненный кадр в полёте: sync (чтобы драйвер корректно прибрался).
        if let Some((coded, _)) = self.prev.take() {
            unsafe { let _ = va_ok(vaSyncBuffer(self.va.dpy, coded, 3_000_000_000), "vaSyncBuffer(drop)"); }
        }
        let dpy = self.va.dpy;
        unsafe {
            for c in self.coded {
                if c != VA_INVALID_ID {
                    vaDestroyBuffer(dpy, c);
                }
            }
            vaDestroySurfaces(dpy, self.vdst.as_mut_ptr(), 2);
            vaDestroySurfaces(dpy, self.recon.as_mut_ptr(), 2);
            vaDestroyContext(dpy, self.ectx);
            vaDestroyContext(dpy, self.vctx);
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    #[test]
    fn print_sizes() {
        eprintln!("seq={} pic={} slc={} packedparam={} codedseg={} picH264={} desc={} gval={} sattr={}",
            std::mem::size_of::<VaEncSequenceParameterBufferH264>(),
            std::mem::size_of::<VaEncPictureParameterBufferH264>(),
            std::mem::size_of::<VaEncSliceParameterBufferH264>(),
            std::mem::size_of::<VaEncPackedHeaderParameterBuffer>(),
            std::mem::size_of::<VaCodedBufferSegment>(),
            std::mem::size_of::<VaPictureH264>(),
            std::mem::size_of::<VaDrmPrimeSurfaceDescriptor>(),
            std::mem::size_of::<VaGenericValue>(),
            std::mem::size_of::<VaSurfaceAttrib>());
    }
}

// ---------- Z4a standalone-пробник (сценарий P3) ----------

/// DRM-ioctl константы (linux/drm_mode.h): _IOWR(nr, size) с типом 'd'=0x64.
const DRM_IOCTL_BASE: u64 = 0x64;
const fn iowr(nr: u64, sz: u64) -> u64 {
    (3u64 << 30) | (sz << 16) | (DRM_IOCTL_BASE << 8) | nr
}
const DRM_IOCTL_MODE_CREATE_DUMB: u64 = iowr(0xB2, 24);
const DRM_IOCTL_PRIME_HANDLE_TO_FD: u64 = iowr(0x2D, 12);
const O_RDWR_CLOEXEC: i32 = 0o2 | 0o2000000; // O_RDWR | O_CLOEXEC

#[repr(C)]
struct DrmCreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
struct DrmPrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
    _reserved: u32,
}

fn ioctl(fd: i32, cmd: u64, arg: *mut std::os::raw::c_void) -> i32 {
    unsafe { libc::ioctl(fd, cmd as libc::c_ulong, arg) }
}

/// Паттерн P3: 8 вертикальных BGRX-полос со сдвигом по X (движение для P-кадров).
fn fill_pattern(p: &mut [u8], pitch: u32, w: u32, h: u32, shift: i32) {
    const BARS: [[u8; 4]; 8] = [
        [0, 0, 255, 0], [0, 255, 0, 0], [255, 0, 0, 0], [0, 255, 255, 0],
        [255, 0, 255, 0], [255, 255, 0, 0], [64, 64, 64, 0], [192, 192, 192, 0],
    ];
    for y in 0..h as usize {
        let row = &mut p[y * pitch as usize..(y * pitch as usize + w as usize * 4)];
        for x in 0..w as usize {
            let c = &BARS[((((x as i64) + shift as i64) % w as i64) * 8 / w as i64) as usize];
            row[x * 4..x * 4 + 4].copy_from_slice(c);
        }
    }
}

/// Z4a: dumb-dmabuf → импорт → VPP → энкодер → /tmp/p3_rs.h264.
/// Гейты (проверяются внешне): 5 кадров без VA-ошибок; NAL-скан + ffmpeg-декод.
pub fn selftest(nframes: usize) -> Result<()> {
    const W: u32 = 1088;
    const H: u32 = 800;
    let gop: u32 = std::env::var("NX_GOP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let bitrate: u32 = std::env::var("NX_BITRATE_KBPS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(|k| k * 1000)
        .unwrap_or(20_000_000);

    let mut card = -1;
    for c in ["/dev/dri/card0", "/dev/dri/card1", "/dev/dri/card2", "/dev/dri/card3"] {
        if !std::path::Path::new(c).exists() { continue; }
        // &str::as_ptr() не NUL-терминирован → нужен CString (иначе open вернёт ENOENT).
        let cs = std::ffi::CString::new(c).unwrap();
        let fd = unsafe { libc::open(cs.as_ptr(), O_RDWR_CLOEXEC) };
        if fd >= 0 { card = fd; break; }
    }
    anyhow::ensure!(card >= 0, "не открыть /dev/dri/card*");

    let mut enc = VaEncoder::new(W, H, W, H, bitrate, gop, 60)?;
    // Два dmabuf-слота, как в боевом capture_loop: пока VCE читает слот N,
    // CPU заполняет слот N+1. Один слот в пробнике создавал гонку «запись в
    // поверхность, которую ещё кодирует VCE» и завышал sync до сотен мс.
    let mut fds = [0i32; 2];
    let mut pitches = [0u32; 2];
    let mut maps: Vec<&mut [u8]> = Vec::new();
    for s in 0..2 {
        let mut dumb = DrmCreateDumb { height: H, width: W, bpp: 32, flags: 0, handle: 0, pitch: 0, size: 0 };
        anyhow::ensure!(ioctl(card, DRM_IOCTL_MODE_CREATE_DUMB, &mut dumb as *mut _ as *mut _) == 0, "CREATE_DUMB {s}");
        let mut ph = DrmPrimeHandle { handle: dumb.handle, flags: O_RDWR_CLOEXEC as u32, fd: -1, _reserved: 0 };
        anyhow::ensure!(ioctl(card, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph as *mut _ as *mut _) == 0, "PRIME_HANDLE_TO_FD {s}");
        let sz = dumb.pitch as usize * H as usize;
        let map = unsafe {
            libc::mmap(std::ptr::null_mut(), sz, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, ph.fd, 0)
        };
        anyhow::ensure!(map != libc::MAP_FAILED, "mmap dmabuf {s}");
        fds[s] = ph.fd;
        pitches[s] = dumb.pitch;
        maps.push(unsafe { std::slice::from_raw_parts_mut(map as *mut u8, sz) });
    }
    enc.import_slot(0, fds[0], pitches[0])?;
    enc.import_slot(1, fds[1], pitches[1])?;

    let mut out = std::fs::File::create("/tmp/p3_rs.h264").context("open /tmp/p3_rs.h264")?;
    use std::io::Write;
    let mut total = 0usize;
    let mut total_ms = 0f64;
    let mut aums: Vec<f64> = Vec::new();
    let period = std::time::Duration::from_nanos(1_000_000_000 / 60);
    let mut next = std::time::Instant::now();
    for i in 0..nframes {
        let slot = i % 2;
        fill_pattern(&mut maps[slot], pitches[slot], W, H, (i as i32) * 136);
        let t0 = std::time::Instant::now();
        let au = enc.encode_slot(slot)?;
        let dt = t0.elapsed().as_secs_f64() * 1000.0;
        total_ms += dt;
        aums.push(dt);
        total += au.len();
        if i < 6 || nframes <= 40 {
            let idr = i % gop as usize == 0;
            println!("frame {} {}: bytes={} encode={:.2} ms", i, if idr { "IDR" } else { "P  " }, au.len(), dt);
        } else if dt >= 20.0 {
            let (vpp, setup, submit, sync) = enc.timing;
            println!("frame {} {}: bytes={} encode={:.2} ms (spike) vpp={:.1} setup={:.1} submit={:.1} sync={:.1}", i, if i % gop as usize == 0 { "IDR" } else { "P  " }, au.len(), dt, vpp, setup, submit, sync);
        }
        out.write_all(&au)?;

        // Реалистичный темп: захват даёт кадры 60/с. Без паузы цикл
        // засабмичивает сотни кадров/с, очередь VCE растёт и замер врёт:
        // sync начинает ждать кадр, который ещё в очереди.
        next += period;
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    println!(
        "done: {} frames, {} bytes, avg {:.2} ms/frame ({:.0} fps)",
        nframes,
        total,
        total_ms / nframes as f64,
        nframes as f64 * 1000.0 / total_ms
    );
    if nframes >= 30 {
        let p: Vec<f64> = aums.clone();
        let mut sorted = p.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = sorted[sorted.len() / 2];
        let p95 = sorted[sorted.len() * 95 / 100];
        let p99 = sorted[sorted.len() * 99 / 100];
        println!("timing: p50={:.2} p95={:.2} p99={:.2} max={:.2} ms", p50, p95, p99, p.iter().cloned().fold(0./1., f64::max));
    }
    Ok(())
}
