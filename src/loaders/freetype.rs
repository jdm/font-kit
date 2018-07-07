// font-kit/src/loaders/freetype.rs
//
// Copyright © 2018 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Support for font loading using FreeType.
//!
//! On macOS and Windows, the Cargo feature `loader-freetype` can be used to opt into this loader.
//! Because on these platforms FreeType can do everything the native loader can do and more, this
//! Cargo feature completely disables native font loading. In particular, this feature enables
//! support for retrieving hinted outlines.

use byteorder::{BigEndian, ReadBytesExt};
use euclid::{Point2D, Rect, Size2D, Vector2D};
use freetype::freetype::{FT_Byte, FT_Done_Face, FT_Error, FT_FACE_FLAG_FIXED_WIDTH};
use freetype::freetype::{FT_FACE_FLAG_VERTICAL, FT_Face, FT_Get_Char_Index};
use freetype::freetype::{FT_Get_Postscript_Name, FT_Get_Sfnt_Table, FT_Init_FreeType};
use freetype::freetype::{FT_LOAD_DEFAULT, FT_LOAD_NO_HINTING, FT_Long};
use freetype::freetype::{FT_Library, FT_Load_Glyph, FT_New_Memory_Face, FT_Reference_Face};
use freetype::freetype::{FT_Set_Char_Size, FT_Sfnt_Tag, FT_STYLE_FLAG_ITALIC};
use freetype::freetype::{FT_UInt, FT_ULong, FT_UShort, FT_Vector};
use freetype::tt_os2::TT_OS2;
use lyon_path::builder::PathBuilder;
use memmap::Mmap;
use std::ffi::CStr;
use std::fmt::{self, Debug, Formatter};
use std::fs::File;
use std::iter;
use std::marker::PhantomData;
use std::mem;
use std::ops::Deref;
use std::os::raw::{c_char, c_void};
use std::ptr;
use std::slice;
use std::sync::Arc;

#[cfg(target_os = "macos")]
use core_text::font::CTFont;

use descriptor::{Descriptor, FONT_STRETCH_MAPPING, Flags};
use font::{Face, Metrics, Type};

const PS_DICT_FULL_NAME: u32 = 38;
const TT_NAME_ID_FULL_NAME: u16 = 4;

const FT_POINT_TAG_ON_CURVE: c_char = 0x01;
const FT_POINT_TAG_CUBIC_CONTROL: c_char = 0x02;

thread_local! {
    static FREETYPE_LIBRARY: FT_Library = {
        unsafe {
            let mut library = ptr::null_mut();
            assert_eq!(FT_Init_FreeType(&mut library), 0);
            library
        }
    };
}

pub type NativeFont = FT_Face;

pub struct Font {
    freetype_face: FT_Face,
    font_data: FontData<'static>,
}

impl Font {
    pub fn from_bytes(font_data: Arc<Vec<u8>>, font_index: u32) -> Result<Font, ()> {
        FREETYPE_LIBRARY.with(|freetype_library| {
            unsafe {
                let mut freetype_face = ptr::null_mut();
                assert_eq!(FT_New_Memory_Face(*freetype_library,
                                              (*font_data).as_ptr(),
                                              font_data.len() as i64,
                                              font_index as FT_Long,
                                              &mut freetype_face),
                           0);
                setup_freetype_face(freetype_face);
                Ok(Font {
                    freetype_face,
                    font_data: FontData::Memory(font_data),
                })
            }
        })
    }

    pub fn from_file(file: File, font_index: u32) -> Result<Font, ()> {
        unsafe {
            let mmap = try!(Mmap::map(&file).map_err(drop));
            FREETYPE_LIBRARY.with(|freetype_library| {
                let mut freetype_face = ptr::null_mut();
                assert_eq!(FT_New_Memory_Face(*freetype_library,
                                              (*mmap).as_ptr(),
                                              mmap.len() as i64,
                                              font_index as FT_Long,
                                              &mut freetype_face),
                           0);
                setup_freetype_face(freetype_face);
                Ok(Font {
                    freetype_face,
                    font_data: FontData::File(Arc::new(mmap)),
                })
            })
        }
    }

    pub unsafe fn from_native_font(freetype_face: NativeFont) -> Font {
        // We make an in-memory copy of the underlying font data. This is because the native font
        // does not necessarily hold a strong reference to the memory backing it.
        const CHUNK_SIZE: usize = 4096;
        let mut font_data = vec![];
        loop {
            font_data.extend(iter::repeat(0).take(CHUNK_SIZE));
            let freetype_stream = (*freetype_face).stream;
            let n_read = ((*freetype_stream).read.unwrap())(freetype_stream,
                                                            font_data.len() as u64,
                                                            font_data.as_mut_ptr(),
                                                            CHUNK_SIZE as u64);
            if n_read < CHUNK_SIZE as u64 {
                break
            }
        }

        Font::from_bytes(Arc::new(font_data), (*freetype_face).face_index as u32).unwrap()
    }

    #[cfg(target_os = "macos")]
    pub unsafe fn from_core_text_font(core_text_font: CTFont) -> Font {
        // FIXME(pcwalton): How do we deal with collections? I guess we have to find which font in
        // the collection matches?
        let path = core_text_font.url().unwrap().to_path().unwrap();
        Font::from_file(File::open(path).unwrap(), 0).unwrap()
    }

    pub fn analyze_bytes(font_data: Arc<Vec<u8>>) -> Type {
        FREETYPE_LIBRARY.with(|freetype_library| {
            unsafe {
                let mut freetype_face = ptr::null_mut();
                if FT_New_Memory_Face(*freetype_library,
                                      (*font_data).as_ptr(),
                                      font_data.len() as i64,
                                      0,
                                      &mut freetype_face) != 0 {
                    return Type::Unsupported
                }
                let font_type = match (*freetype_face).num_faces {
                    1 => Type::Single,
                    num_faces => Type::Collection(num_faces as u32),
                };
                FT_Done_Face(freetype_face);
                font_type
            }
        })
    }

    pub fn analyze_file(file: File) -> Type {
        FREETYPE_LIBRARY.with(|freetype_library| {
            unsafe {
                let mmap = match Mmap::map(&file) {
                    Ok(mmap) => mmap,
                    Err(_) => return Type::Unsupported,
                };
                let mut freetype_face = ptr::null_mut();
                if FT_New_Memory_Face(*freetype_library,
                                      (*mmap).as_ptr(),
                                      mmap.len() as i64,
                                      0,
                                      &mut freetype_face) != 0 {
                    return Type::Unsupported
                }
                let font_type = match (*freetype_face).num_faces {
                    1 => Type::Single,
                    num_faces => Type::Collection(num_faces as u32),
                };
                FT_Done_Face(freetype_face);
                font_type
            }
        })
    }

    pub fn descriptor(&self) -> Descriptor {
        unsafe {
            let postscript_name = FT_Get_Postscript_Name(self.freetype_face);
            let postscript_name = CStr::from_ptr(postscript_name).to_str().unwrap().to_owned();
            let family_name = CStr::from_ptr((*self.freetype_face).family_name).to_str()
                                                                               .unwrap()
                                                                               .to_owned();
            let style_name = CStr::from_ptr((*self.freetype_face).style_name).to_str()
                                                                             .unwrap()
                                                                             .to_owned();
            let display_name = self.get_type_1_or_sfnt_name(PS_DICT_FULL_NAME,
                                                            TT_NAME_ID_FULL_NAME)
                                   .unwrap_or_else(|| family_name.clone());
            let os2_table = self.get_os2_table();

            let mut flags = Flags::empty();
            flags.set(Flags::ITALIC,
                      ((*self.freetype_face).style_flags & (FT_STYLE_FLAG_ITALIC as i64)) != 0);
            flags.set(Flags::MONOSPACE,
                      (*self.freetype_face).face_flags & (FT_FACE_FLAG_FIXED_WIDTH as i64) != 0);
            flags.set(Flags::VERTICAL,
                      (*self.freetype_face).face_flags & (FT_FACE_FLAG_VERTICAL as i64) != 0);

            Descriptor {
                postscript_name,
                display_name,
                family_name,
                style_name,
                stretch: FONT_STRETCH_MAPPING[((*os2_table).usWidthClass as usize) - 1],
                weight: (*os2_table).usWeightClass as u32 as f32,
                flags,
            }
        }
    }

    pub fn glyph_for_char(&self, character: char) -> Option<u32> {
        unsafe {
            Some(FT_Get_Char_Index(self.freetype_face, character as FT_ULong))
        }
    }

    pub fn outline<B>(&self, glyph_id: u32, path_builder: &mut B) -> Result<(), ()>
                      where B: PathBuilder {
        unsafe {
            assert_eq!(FT_Load_Glyph(self.freetype_face,
                                     glyph_id,
                                     (FT_LOAD_DEFAULT | FT_LOAD_NO_HINTING) as i32),
                       0);

            let outline = &(*(*self.freetype_face).glyph).outline;
            let contours = slice::from_raw_parts((*outline).contours,
                                                 (*outline).n_contours as usize);
            let point_positions = slice::from_raw_parts((*outline).points,
                                                        (*outline).n_points as usize);
            let point_tags = slice::from_raw_parts((*outline).tags, (*outline).n_points as usize);

            let mut current_point_index = 0;
            for &last_point_index_in_contour in contours {
                let last_point_index_in_contour = last_point_index_in_contour as usize;
                let (point, _) = get_point(&mut current_point_index,
                                           point_positions,
                                           point_tags,
                                           last_point_index_in_contour);
                path_builder.move_to(point);
                while current_point_index <= last_point_index_in_contour {
                    let (point0, tag) = get_point(&mut current_point_index,
                                                  point_positions,
                                                  point_tags,
                                                  last_point_index_in_contour);
                    if (tag & FT_POINT_TAG_ON_CURVE) != 0 {
                        path_builder.line_to(point0)
                    } else {
                        let (point1, _) = get_point(&mut current_point_index,
                                                    point_positions,
                                                    point_tags,
                                                    last_point_index_in_contour);
                        if (tag & FT_POINT_TAG_CUBIC_CONTROL) != 0 {
                            let (point2, _) = get_point(&mut current_point_index,
                                                        point_positions,
                                                        point_tags,
                                                        last_point_index_in_contour);
                            path_builder.cubic_bezier_to(point0, point1, point2)
                        } else {
                            path_builder.quadratic_bezier_to(point0, point1)
                        }
                    }
                }
                path_builder.close();
            }
        }
        return Ok(());

        fn get_point(current_point_index: &mut usize,
                     point_positions: &[FT_Vector],
                     point_tags: &[c_char],
                     last_point_index_in_contour: usize)
                     -> (Point2D<f32>, c_char) {
            assert!(*current_point_index <= last_point_index_in_contour);
            let point_position = point_positions[*current_point_index];
            let point_tag = point_tags[*current_point_index];
            *current_point_index += 1;
            let point_position = Point2D::new(ft_fixed_26_6_to_f32(point_position.x),
                                              ft_fixed_26_6_to_f32(point_position.y));
            (point_position, point_tag)
        }
    }

    pub fn typographic_bounds(&self, glyph_id: u32) -> Rect<f32> {
        unsafe {
            assert_eq!(FT_Load_Glyph(self.freetype_face,
                                     glyph_id,
                                     (FT_LOAD_DEFAULT | FT_LOAD_NO_HINTING) as i32),
                       0);
            let metrics = &(*(*self.freetype_face).glyph).metrics;
            Rect::new(Point2D::new(ft_fixed_26_6_to_f32(metrics.horiBearingX),
                                   ft_fixed_26_6_to_f32(metrics.horiBearingY - metrics.height)),
                      Size2D::new(ft_fixed_26_6_to_f32(metrics.width),
                                  ft_fixed_26_6_to_f32(metrics.height)))
        }
    }

    pub fn advance(&self, glyph_id: u32) -> Vector2D<f32> {
        unsafe {
            assert_eq!(FT_Load_Glyph(self.freetype_face,
                                     glyph_id,
                                     (FT_LOAD_DEFAULT | FT_LOAD_NO_HINTING) as i32),
                       0);
            let advance = (*(*self.freetype_face).glyph).advance;
            Vector2D::new(ft_fixed_26_6_to_f32(advance.x), ft_fixed_26_6_to_f32(advance.y))
        }
    }

    pub fn origin(&self, _: u32) -> Point2D<f32> {
        // FIXME(pcwalton): This can't be right!
        Point2D::zero()
    }

    pub fn metrics(&self) -> Metrics {
        let os2_table = self.get_os2_table();
        unsafe {
            let ascender = (*self.freetype_face).ascender;
            let descender = (*self.freetype_face).descender;
            let underline_position = (*self.freetype_face).underline_position;
            let underline_thickness = (*self.freetype_face).underline_thickness;
            Metrics {
                units_per_em: (*self.freetype_face).units_per_EM as u32,
                ascent: ascender as f32,
                descent: descender as f32,
                line_gap: ((*self.freetype_face).height + descender - ascender) as f32,
                underline_position: (underline_position + underline_thickness / 2) as f32,
                underline_thickness: underline_thickness as f32,
                cap_height: (*os2_table).sCapHeight as f32,
                x_height: (*os2_table).sxHeight as f32,
            }
        }
    }

    #[inline]
    pub fn font_data(&self) -> Option<FontData> {
        match self.font_data {
            FontData::File(_) | FontData::Memory(_) => Some(self.font_data.clone()),
            FontData::Unused(_) => unreachable!(),
        }
    }

    fn get_type_1_or_sfnt_name(&self, type_1_id: u32, sfnt_id: u16) -> Option<String> {
        unsafe {
            let ps_value_size = FT_Get_PS_Font_Value(self.freetype_face,
                                                     type_1_id,
                                                     0,
                                                     ptr::null_mut(),
                                                     0);
            if ps_value_size > 0 {
                let mut buffer = vec![0; ps_value_size as usize];
                if FT_Get_PS_Font_Value(self.freetype_face,
                                        type_1_id,
                                        0,
                                        buffer.as_mut_ptr() as *mut c_void,
                                        buffer.len() as i64) == 0 {
                    return String::from_utf8(buffer).ok()
                }
            }

            let sfnt_name_count = FT_Get_Sfnt_Name_Count(self.freetype_face);
            let mut sfnt_name = mem::zeroed();
            for sfnt_name_index in 0..sfnt_name_count {
                assert_eq!(FT_Get_Sfnt_Name(self.freetype_face, sfnt_name_index, &mut sfnt_name),
                           0);
                // FIXME(pcwalton): Check encoding, platform, language. It isn't always UTF-16…
                if sfnt_name.name_id != sfnt_id {
                    continue
                }

                let mut sfnt_name_bytes = slice::from_raw_parts(sfnt_name.string,
                                                                sfnt_name.string_len as usize);
                let mut sfnt_name_string = Vec::with_capacity(sfnt_name_bytes.len() / 2);
                while !sfnt_name_bytes.is_empty() {
                    sfnt_name_string.push(sfnt_name_bytes.read_u16::<BigEndian>().unwrap())
                }

                if let Ok(result) = String::from_utf16(&sfnt_name_string) {
                    return Some(result)
                }
            }

            None
        }
    }

    fn get_os2_table(&self) -> *const TT_OS2 {
        unsafe {
            FT_Get_Sfnt_Table(self.freetype_face, FT_Sfnt_Tag::FT_SFNT_OS2) as *const TT_OS2
        }
    }
}

impl Clone for Font {
    fn clone(&self) -> Font {
        unsafe {
            assert_eq!(FT_Reference_Face(self.freetype_face), 0);
            Font {
                freetype_face: self.freetype_face,
                font_data: self.font_data.clone(),
            }
        }
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        unsafe {
            if !self.freetype_face.is_null() {
                assert_eq!(FT_Done_Face(self.freetype_face), 0);
            }
        }
    }
}

impl Debug for Font {
    fn fmt(&self, fmt: &mut Formatter) -> Result<(), fmt::Error> {
        self.descriptor().fmt(fmt)
    }
}

impl Face for Font {
    type NativeFont = NativeFont;

    #[inline]
    fn from_bytes(font_data: Arc<Vec<u8>>, font_index: u32) -> Result<Self, ()> {
        Font::from_bytes(font_data, font_index)
    }

    #[inline]
    fn from_file(file: File, font_index: u32) -> Result<Font, ()> {
        Font::from_file(file, font_index)
    }

    #[inline]
    unsafe fn from_native_font(native_font: Self::NativeFont) -> Self {
        Font::from_native_font(native_font)
    }

    #[cfg(target_os = "macos")]
    #[inline]
    unsafe fn from_core_text_font(core_text_font: CTFont) -> Font {
        Font::from_core_text_font(core_text_font)
    }

    #[inline]
    fn descriptor(&self) -> Descriptor {
        self.descriptor()
    }

    #[inline]
    fn glyph_for_char(&self, character: char) -> Option<u32> {
        self.glyph_for_char(character)
    }

    #[inline]
    fn outline<B>(&self, glyph_id: u32, path_builder: &mut B) -> Result<(), ()>
                  where B: PathBuilder {
        self.outline(glyph_id, path_builder)
    }

    #[inline]
    fn typographic_bounds(&self, glyph_id: u32) -> Rect<f32> {
        self.typographic_bounds(glyph_id)
    }

    #[inline]
    fn advance(&self, glyph_id: u32) -> Vector2D<f32> {
        self.advance(glyph_id)
    }

    #[inline]
    fn origin(&self, origin: u32) -> Point2D<f32> {
        self.origin(origin)
    }

    #[inline]
    fn metrics(&self) -> Metrics {
        self.metrics()
    }
}

#[derive(Clone)]
pub enum FontData<'a> {
    Memory(Arc<Vec<u8>>),
    File(Arc<Mmap>),
    Unused(PhantomData<&'a u8>),
}

impl<'a> Deref for FontData<'a> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match *self {
            FontData::File(ref mmap) => &***mmap,
            FontData::Memory(ref data) => &***data,
            FontData::Unused(_) => unreachable!(),
        }
    }
}

unsafe fn setup_freetype_face(face: FT_Face) {
    assert_eq!(FT_Set_Char_Size(face, ((*face).units_per_EM as i64) << 6, 0, 0, 0), 0);
}

#[repr(C)]
struct FT_SfntName {
    platform_id: FT_UShort,
    encoding_id: FT_UShort,
    language_id: FT_UShort,
    name_id: FT_UShort,
    string: *mut FT_Byte,
    string_len: FT_UInt,
}

fn ft_fixed_26_6_to_f32(fixed: i64) -> f32 {
    (fixed as f32) / 64.0
}

extern "C" {
    fn FT_Get_PS_Font_Value(face: FT_Face,
                            key: u32,
                            idx: FT_UInt,
                            value: *mut c_void,
                            value_len: FT_Long)
                            -> FT_Long;
    fn FT_Get_Sfnt_Name(face: FT_Face, idx: FT_UInt, aname: *mut FT_SfntName) -> FT_Error;
    fn FT_Get_Sfnt_Name_Count(face: FT_Face) -> FT_UInt;
}
