//! Kitty Graphics Protocol implementation for zellij
//!
//! This module implements the Kitty graphics protocol for displaying images inline
//! in terminal panes. It handles APC escape sequences since the VTE crate doesn't
//! support them natively.
//!
//! Protocol spec: https://sw.kovidgoyal.net/kitty/graphics-protocol/

use crate::output::KittyImageChunk;
use crate::panes::sixel::PixelRect;
use flate2::read::ZlibDecoder;
use image::ImageFormat;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::rc::Rc;
use zellij_utils::pane_size::SizeInPixels;

/// Kitty graphics action types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KittyAction {
    /// Transmit image data (a=t)
    Transmit,
    /// Transmit and display (a=T) - default
    #[default]
    TransmitPut,
    /// Query terminal support (a=q)
    Query,
    /// Display previously transmitted image (a=p)
    Put,
    /// Delete images (a=d)
    Delete,
    /// Transmit animation frame (a=f)
    Frame,
    /// Control animation (a=a)
    Animation,
    /// Compose frames (a=c)
    Compose,
}

/// Image data format
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KittyFormat {
    /// Raw RGB (f=24)
    Rgb24,
    /// Raw RGBA (f=32) - default
    #[default]
    Rgba32,
    /// PNG compressed (f=100)
    Png,
}

/// Transmission medium
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransmissionMedium {
    /// Direct data in escape sequence (t=d) - default
    #[default]
    Direct,
    /// Read from file (t=f)
    File,
    /// Read from temp file and delete (t=t)
    TempFile,
    /// Read from shared memory (t=s)
    SharedMemory,
}

/// Parsed control data from a kitty graphics command
#[derive(Debug, Clone, Default)]
pub struct KittyControlData {
    pub action: KittyAction,
    pub format: KittyFormat,
    pub transmission: TransmissionMedium,
    pub image_id: Option<u32>,
    pub image_number: Option<u32>,
    pub placement_id: Option<u32>,
    /// Source image width in pixels (s=)
    pub width: Option<usize>,
    /// Source image height in pixels (v=)
    pub height: Option<usize>,
    /// Data size in bytes (S=)
    pub data_size: Option<usize>,
    /// Data offset (O=)
    pub data_offset: Option<usize>,
    /// More data chunks coming (m=1)
    pub more_data: bool,
    /// Compression type ('z' for zlib)
    pub compression: Option<char>,
    /// Quiet mode (0-2)
    pub quiet: u8,
    /// Display X offset in pixels (x=)
    pub display_x: Option<usize>,
    /// Display Y offset in pixels (y=)
    pub display_y: Option<usize>,
    /// Display width in pixels (w=)
    pub display_width: Option<usize>,
    /// Display height in pixels (h=)
    pub display_height: Option<usize>,
    /// Columns to occupy (c=)
    pub columns: Option<usize>,
    /// Rows to occupy (r=)
    pub rows: Option<usize>,
    /// Z-index for layering (z=)
    pub z_index: Option<i32>,
    /// Delete type for delete action (d=)
    pub delete_type: Option<char>,
    /// Don't move cursor after display (C=1)
    pub cursor_movement: bool,
}

/// A parsed kitty graphics command
#[derive(Debug, Clone)]
pub struct KittyCommand {
    pub control: KittyControlData,
    pub payload: Vec<u8>,
}

/// State for chunked image transmission
#[derive(Debug, Clone)]
struct ChunkedTransmission {
    control: KittyControlData,
    accumulated_payload: Vec<u8>,
}

/// A decoded kitty image ready for display
#[derive(Debug, Clone)]
pub struct KittyImage {
    pub id: u32,
    pub width: usize,
    pub height: usize,
    pub pixel_data: Vec<u8>, // RGBA format
}

/// An image placement (instance of an image at a location)
#[derive(Debug, Clone)]
pub struct KittyPlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub pixel_x: usize,
    pub pixel_y: isize,
    pub display_width: usize,
    pub display_height: usize,
    pub z_index: i32,
}

/// APC parser state machine
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApcParseState {
    #[default]
    Ground,
    EscapeSeen,
    ApcEntry,
    ApcParam,
    ApcPassthrough,
    StringTerminator,
}

/// Result from advancing the APC parser
#[derive(Debug)]
pub enum ApcAdvanceResult {
    /// Continue buffering bytes
    Continue,
    /// Complete kitty graphics command parsed
    Complete(KittyCommand),
    /// Not an APC or not a kitty graphics sequence, forward these bytes to VTE
    NotApc(Vec<u8>),
    /// Parse error, reset state
    Error,
}

/// APC parser for detecting and parsing kitty graphics sequences
#[derive(Debug, Clone, Default)]
pub struct KittyApcParser {
    state: ApcParseState,
    control_buffer: Vec<u8>,
    payload_buffer: Vec<u8>,
    escape_buffer: Vec<u8>,
}

impl KittyApcParser {
    /// Check if currently parsing an APC sequence
    pub fn is_parsing(&self) -> bool {
        self.state != ApcParseState::Ground
    }

    /// Process a single byte
    pub fn advance(&mut self, byte: u8) -> ApcAdvanceResult {
        match self.state {
            ApcParseState::Ground => {
                if byte == 0x1B {
                    // ESC
                    self.state = ApcParseState::EscapeSeen;
                    self.escape_buffer.push(byte);
                    ApcAdvanceResult::Continue
                } else {
                    ApcAdvanceResult::NotApc(vec![byte])
                }
            }
            ApcParseState::EscapeSeen => {
                if byte == b'_' {
                    // APC introducer (ESC _)
                    self.state = ApcParseState::ApcEntry;
                    self.escape_buffer.clear();
                    ApcAdvanceResult::Continue
                } else {
                    // Not APC, return buffered ESC + this byte
                    let mut buffered = std::mem::take(&mut self.escape_buffer);
                    buffered.push(byte);
                    self.state = ApcParseState::Ground;
                    ApcAdvanceResult::NotApc(buffered)
                }
            }
            ApcParseState::ApcEntry => {
                if byte == b'G' {
                    // Kitty graphics command
                    self.state = ApcParseState::ApcParam;
                    ApcAdvanceResult::Continue
                } else {
                    // Unknown APC type, skip until ST
                    // For now, just reset and pass through
                    self.state = ApcParseState::Ground;
                    ApcAdvanceResult::NotApc(vec![0x1B, b'_', byte])
                }
            }
            ApcParseState::ApcParam => {
                if byte == b';' {
                    // End of control data, start of payload
                    self.state = ApcParseState::ApcPassthrough;
                    ApcAdvanceResult::Continue
                } else if byte == 0x1B {
                    // Potential string terminator
                    self.state = ApcParseState::StringTerminator;
                    ApcAdvanceResult::Continue
                } else if byte == 0x07 {
                    // BEL also terminates (no payload case)
                    self.finalize_command()
                } else {
                    self.control_buffer.push(byte);
                    ApcAdvanceResult::Continue
                }
            }
            ApcParseState::ApcPassthrough => {
                if byte == 0x1B {
                    self.state = ApcParseState::StringTerminator;
                    ApcAdvanceResult::Continue
                } else if byte == 0x07 {
                    // BEL terminates
                    self.finalize_command()
                } else {
                    self.payload_buffer.push(byte);
                    ApcAdvanceResult::Continue
                }
            }
            ApcParseState::StringTerminator => {
                if byte == b'\\' {
                    // Complete ST (ESC \)
                    self.finalize_command()
                } else {
                    // False alarm, ESC was part of payload
                    self.payload_buffer.push(0x1B);
                    self.payload_buffer.push(byte);
                    self.state = ApcParseState::ApcPassthrough;
                    ApcAdvanceResult::Continue
                }
            }
        }
    }

    fn finalize_command(&mut self) -> ApcAdvanceResult {
        let control_bytes = std::mem::take(&mut self.control_buffer);
        let payload = std::mem::take(&mut self.payload_buffer);
        self.state = ApcParseState::Ground;

        log::debug!(
            "Kitty APC: Finalizing command - control_len={}, payload_len={}",
            control_bytes.len(),
            payload.len()
        );

        match parse_control_data(&control_bytes) {
            Ok(control) => {
                log::debug!(
                    "Kitty APC: Parsed control data - action={:?}, format={:?}, image_id={:?}",
                    control.action,
                    control.format,
                    control.image_id
                );
                ApcAdvanceResult::Complete(KittyCommand { control, payload })
            }
            Err(_) => {
                log::warn!(
                    "Kitty APC: Failed to parse control data: {:?}",
                    String::from_utf8_lossy(&control_bytes)
                );
                ApcAdvanceResult::Error
            }
        }
    }

    /// Reset parser state
    pub fn reset(&mut self) {
        self.state = ApcParseState::Ground;
        self.control_buffer.clear();
        self.payload_buffer.clear();
        self.escape_buffer.clear();
    }
}

/// Parse control data string like "a=T,f=100,i=1"
fn parse_control_data(data: &[u8]) -> Result<KittyControlData, ()> {
    let s = std::str::from_utf8(data).map_err(|_| ())?;
    let mut control = KittyControlData::default();

    for pair in s.split(',') {
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().ok_or(())?;
        let value = parts.next().unwrap_or("");

        match key {
            "a" => {
                control.action = match value {
                    "t" => KittyAction::Transmit,
                    "T" => KittyAction::TransmitPut,
                    "q" => KittyAction::Query,
                    "p" => KittyAction::Put,
                    "d" => KittyAction::Delete,
                    "f" => KittyAction::Frame,
                    "a" => KittyAction::Animation,
                    "c" => KittyAction::Compose,
                    _ => KittyAction::TransmitPut,
                }
            }
            "f" => {
                control.format = match value {
                    "24" => KittyFormat::Rgb24,
                    "32" => KittyFormat::Rgba32,
                    "100" => KittyFormat::Png,
                    _ => KittyFormat::Rgba32,
                }
            }
            "t" => {
                control.transmission = match value {
                    "d" => TransmissionMedium::Direct,
                    "f" => TransmissionMedium::File,
                    "t" => TransmissionMedium::TempFile,
                    "s" => TransmissionMedium::SharedMemory,
                    _ => TransmissionMedium::Direct,
                }
            }
            "i" => control.image_id = value.parse().ok(),
            "I" => control.image_number = value.parse().ok(),
            "p" => control.placement_id = value.parse().ok(),
            "s" => control.width = value.parse().ok(),
            "v" => control.height = value.parse().ok(),
            "S" => control.data_size = value.parse().ok(),
            "O" => control.data_offset = value.parse().ok(),
            "m" => control.more_data = value == "1",
            "o" => control.compression = value.chars().next(),
            "q" => control.quiet = value.parse().unwrap_or(0),
            "x" => control.display_x = value.parse().ok(),
            "y" => control.display_y = value.parse().ok(),
            "w" => control.display_width = value.parse().ok(),
            "h" => control.display_height = value.parse().ok(),
            "c" => control.columns = value.parse().ok(),
            "r" => control.rows = value.parse().ok(),
            "z" => control.z_index = value.parse().ok(),
            "d" => control.delete_type = value.chars().next(),
            "C" => control.cursor_movement = value == "1",
            _ => {} // Unknown keys are ignored per protocol
        }
    }

    Ok(control)
}

/// Image store for kitty graphics
pub type KittyImageCache = HashMap<PixelRect, Vec<u8>>;

#[derive(Debug, Clone, Default)]
pub struct KittyImageStore {
    images: HashMap<u32, (KittyImage, KittyImageCache)>,
}

impl KittyImageStore {
    /// Store a new image
    pub fn store_image(&mut self, image: KittyImage) {
        let id = image.id;
        self.images.insert(id, (image, HashMap::new()));
    }

    /// Get an image by ID
    pub fn get_image(&self, id: u32) -> Option<&KittyImage> {
        self.images.get(&id).map(|(img, _)| img)
    }

    /// Remove an image
    pub fn remove_image(&mut self, id: u32) {
        self.images.remove(&id);
    }

    /// Delete images based on delete type
    pub fn delete(&mut self, delete_type: char, id: Option<u32>) {
        match delete_type {
            'a' | 'A' => self.images.clear(),
            'i' | 'I' => {
                if let Some(id) = id {
                    self.images.remove(&id);
                }
            }
            _ => {} // Other delete types handled at placement level
        }
    }

    /// Serialize an image region to kitty graphics format for output
    /// pixel_x/y: offset within the source image
    /// pixel_width/height: the display dimensions (how large to render)
    pub fn serialize_image(
        &mut self,
        image_id: u32,
        pixel_x: usize,
        pixel_y: usize,
        pixel_width: usize,
        pixel_height: usize,
    ) -> Option<String> {
        log::debug!(
            "Kitty: Serializing image - id={}, offset=({}, {}), size={}x{}",
            image_id,
            pixel_x,
            pixel_y,
            pixel_width,
            pixel_height
        );

        let (image, cache) = match self.images.get_mut(&image_id) {
            Some(data) => data,
            None => {
                log::warn!("Kitty: Cannot serialize - image_id={} not found in store", image_id);
                return None;
            }
        };

        let cache_key = PixelRect::new(pixel_x, pixel_y, pixel_height, pixel_width);

        // Calculate actual source dimensions (may be smaller than display if image is smaller)
        let actual_source_width = std::cmp::min(pixel_width, image.width.saturating_sub(pixel_x));
        let actual_source_height = std::cmp::min(pixel_height, image.height.saturating_sub(pixel_y));

        if let Some(cached) = cache.get(&cache_key) {
            // Return cached serialization
            return Some(format_kitty_output(
                image_id,
                actual_source_width,
                actual_source_height,
                pixel_width,
                pixel_height,
                cached,
            ));
        }

        // Extract the requested region from the image
        let region_data =
            extract_image_region(image, pixel_x, pixel_y, pixel_width, pixel_height)?;

        // Cache for future use
        cache.insert(cache_key, region_data.clone());

        Some(format_kitty_output(
            image_id,
            actual_source_width,
            actual_source_height,
            pixel_width,
            pixel_height,
            &region_data,
        ))
    }

    pub fn image_count(&self) -> usize {
        self.images.len()
    }
}

/// Extract a region from an image as RGBA bytes
fn extract_image_region(
    image: &KittyImage,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Option<Vec<u8>> {
    if x >= image.width || y >= image.height {
        return None;
    }

    let actual_width = std::cmp::min(width, image.width - x);
    let actual_height = std::cmp::min(height, image.height - y);

    let mut region = Vec::with_capacity(actual_width * actual_height * 4);

    for row in y..(y + actual_height) {
        let row_start = (row * image.width + x) * 4;
        let row_end = row_start + actual_width * 4;
        if row_end <= image.pixel_data.len() {
            region.extend_from_slice(&image.pixel_data[row_start..row_end]);
        }
    }

    Some(region)
}

/// Format RGBA data as a kitty graphics output command
fn format_kitty_output(
    image_id: u32,
    source_width: usize,
    source_height: usize,
    display_width: usize,
    display_height: usize,
    rgba_data: &[u8],
) -> String {
    const CHUNK_SIZE: usize = 4096;
    let encoded = base64::encode(rgba_data);
    let total_chunks = (encoded.len() + CHUNK_SIZE - 1) / CHUNK_SIZE;

    let mut output = String::new();
    for (i, chunk) in encoded.as_bytes().chunks(CHUNK_SIZE).enumerate() {
        let chunk_str = std::str::from_utf8(chunk).unwrap_or("");
        let m = if i == total_chunks - 1 { 0 } else { 1 };

        if i == 0 {
            output.push_str(&format!(
                "\x1b_Ga=T,f=32,s={},v={},w={},h={},i={},m={};{}\x1b\\",
                source_width, source_height, display_width, display_height, image_id, m, chunk_str
            ));
        } else {
            output.push_str(&format!("\x1b_Gm={};{}\x1b\\", m, chunk_str));
        }
    }
    output
}

/// Main grid for managing kitty graphics
#[derive(Debug, Clone, Default)]
pub struct KittyGrid {
    /// Image placement locations
    placements: HashMap<(u32, u32), KittyPlacement>, // (image_id, placement_id) -> placement
    /// Current chunked transmission in progress
    current_transmission: Option<ChunkedTransmission>,
    /// Next available image ID (for auto-assignment)
    next_image_id: u32,
    /// Next available placement ID
    next_placement_id: u32,
    /// Character cell size for coordinate conversion
    character_cell_size: Rc<RefCell<Option<SizeInPixels>>>,
    /// Previous cell size for detecting changes
    previous_cell_size: Option<SizeInPixels>,
    /// Shared image store
    pub kitty_image_store: Rc<RefCell<KittyImageStore>>,
    /// Placement IDs to remove
    placements_to_reap: Vec<(u32, u32)>,
}

impl KittyGrid {
    pub fn new(
        character_cell_size: Rc<RefCell<Option<SizeInPixels>>>,
        kitty_image_store: Rc<RefCell<KittyImageStore>>,
    ) -> Self {
        let previous_cell_size = *character_cell_size.borrow();
        KittyGrid {
            character_cell_size,
            previous_cell_size,
            kitty_image_store,
            next_image_id: 1,
            next_placement_id: 1,
            ..Default::default()
        }
    }

    /// Handle a parsed kitty graphics command, returns response if needed
    pub fn handle_command(
        &mut self,
        cmd: KittyCommand,
        cursor_x_pixels: usize,
        cursor_y_pixels: usize,
    ) -> Option<Vec<u8>> {
        log::debug!(
            "Kitty: Handling command - action={:?}, cursor_pos=({}, {})",
            cmd.control.action,
            cursor_x_pixels,
            cursor_y_pixels
        );
        match cmd.control.action {
            KittyAction::Query => self.handle_query(&cmd),
            KittyAction::Transmit => self.handle_transmit(cmd, false, cursor_x_pixels, cursor_y_pixels),
            KittyAction::TransmitPut => self.handle_transmit(cmd, true, cursor_x_pixels, cursor_y_pixels),
            KittyAction::Put => self.handle_put(&cmd, cursor_x_pixels, cursor_y_pixels),
            KittyAction::Delete => self.handle_delete(&cmd),
            KittyAction::Frame | KittyAction::Animation | KittyAction::Compose => {
                log::debug!("Kitty: Animation features not yet implemented");
                None
            }
        }
    }

    fn handle_query(&self, cmd: &KittyCommand) -> Option<Vec<u8>> {
        log::debug!("Kitty: Handling query command, image_id={:?}", cmd.control.image_id);
        // Respond that we support kitty graphics
        if cmd.control.quiet == 0 {
            let response = format!("\x1b_Gi={};OK\x1b\\", cmd.control.image_id.unwrap_or(0));
            log::debug!("Kitty: Sending query response OK");
            Some(response.into_bytes())
        } else {
            log::debug!("Kitty: Query in quiet mode, no response sent");
            None
        }
    }

    fn handle_transmit(
        &mut self,
        cmd: KittyCommand,
        display: bool,
        cursor_x_pixels: usize,
        cursor_y_pixels: usize,
    ) -> Option<Vec<u8>> {
        log::debug!(
            "Kitty: Handling transmit - display={}, more_data={}, payload_len={}",
            display,
            cmd.control.more_data,
            cmd.payload.len()
        );
        if cmd.control.more_data {
            // Accumulate chunk
            log::debug!("Kitty: Accumulating chunk (more data expected)");
            self.accumulate_chunk(cmd);
            None
        } else {
            // Finalize transmission
            log::debug!("Kitty: Finalizing transmission");
            let final_cmd = self.finalize_transmission(cmd);
            self.process_complete_transmission(final_cmd, display, cursor_x_pixels, cursor_y_pixels)
        }
    }

    fn accumulate_chunk(&mut self, cmd: KittyCommand) {
        if let Some(ref mut transmission) = self.current_transmission {
            // Append to existing transmission
            transmission.accumulated_payload.extend(&cmd.payload);
        } else {
            // Start new chunked transmission
            self.current_transmission = Some(ChunkedTransmission {
                control: cmd.control,
                accumulated_payload: cmd.payload,
            });
        }
    }

    fn finalize_transmission(&mut self, cmd: KittyCommand) -> KittyCommand {
        if let Some(mut transmission) = self.current_transmission.take() {
            // Append final chunk
            transmission.accumulated_payload.extend(&cmd.payload);
            KittyCommand {
                control: transmission.control,
                payload: transmission.accumulated_payload,
            }
        } else {
            cmd
        }
    }

    fn process_complete_transmission(
        &mut self,
        cmd: KittyCommand,
        display: bool,
        cursor_x_pixels: usize,
        cursor_y_pixels: usize,
    ) -> Option<Vec<u8>> {
        log::debug!(
            "Kitty: Processing complete transmission - format={:?}, transmission={:?}, payload_len={}",
            cmd.control.format,
            cmd.control.transmission,
            cmd.payload.len()
        );

        // Decode the image
        let image = match self.decode_image(&cmd) {
            Some(img) => {
                log::debug!(
                    "Kitty: Image decoded successfully - width={}, height={}, pixel_data_len={}",
                    img.width,
                    img.height,
                    img.pixel_data.len()
                );
                img
            }
            None => {
                log::warn!("Kitty: Failed to decode image");
                return None;
            }
        };

        // Assign ID
        let image_id = cmd.control.image_id.unwrap_or_else(|| {
            let id = self.next_image_id;
            self.next_image_id += 1;
            id
        });
        log::debug!("Kitty: Assigned image_id={}", image_id);

        let image = KittyImage {
            id: image_id,
            ..image
        };

        // Store the image
        log::debug!(
            "Kitty: Storing image - id={}, dimensions={}x{}",
            image_id,
            image.width,
            image.height
        );
        self.kitty_image_store.borrow_mut().store_image(image.clone());
        log::debug!(
            "Kitty: Image store now contains {} images",
            self.kitty_image_store.borrow().image_count()
        );

        // Create placement if requested
        if display {
            log::debug!("Kitty: Creating placement for image_id={}", image_id);
            self.create_placement(
                image_id,
                image.width,
                image.height,
                &cmd.control,
                cursor_x_pixels,
                cursor_y_pixels,
            );
        }

        // Send response if not quiet
        if cmd.control.quiet == 0 {
            let response = format!("\x1b_Gi={};OK\x1b\\", image_id);
            log::debug!("Kitty: Sending transmit response OK for image_id={}", image_id);
            Some(response.into_bytes())
        } else {
            log::debug!("Kitty: Transmit in quiet mode, no response sent");
            None
        }
    }

    fn decode_image(&self, cmd: &KittyCommand) -> Option<KittyImage> {
        log::debug!(
            "Kitty: Decoding image - format={:?}, transmission={:?}, compression={:?}",
            cmd.control.format,
            cmd.control.transmission,
            cmd.control.compression
        );

        // Get raw data based on transmission medium
        let raw_data = match cmd.control.transmission {
            TransmissionMedium::Direct => {
                // Direct: payload is base64-encoded image data
                log::debug!("Kitty: Decoding base64 payload (direct transmission)");
                match base64::decode(&cmd.payload) {
                    Ok(data) => {
                        log::debug!("Kitty: Base64 decoded {} bytes", data.len());
                        data
                    }
                    Err(e) => {
                        log::warn!("Kitty: Failed to decode base64 payload: {:?}", e);
                        return None;
                    }
                }
            }
            TransmissionMedium::File => {
                // File: payload is base64-encoded file path
                let path_bytes = match base64::decode(&cmd.payload) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        log::warn!("Kitty: Failed to decode file path from base64: {:?}", e);
                        return None;
                    }
                };
                let path = match std::str::from_utf8(&path_bytes) {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!("Kitty: Invalid UTF-8 in file path: {:?}", e);
                        return None;
                    }
                };
                log::debug!("Kitty: Reading image from file: {}", path);
                match std::fs::read(path) {
                    Ok(data) => {
                        log::debug!("Kitty: Read {} bytes from file", data.len());
                        data
                    }
                    Err(e) => {
                        log::warn!("Kitty: Failed to read file '{}': {:?}", path, e);
                        return None;
                    }
                }
            }
            TransmissionMedium::TempFile => {
                // TempFile: payload is base64-encoded file path, delete after reading
                let path_bytes = match base64::decode(&cmd.payload) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        log::warn!("Kitty: Failed to decode temp file path from base64: {:?}", e);
                        return None;
                    }
                };
                let path = match std::str::from_utf8(&path_bytes) {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!("Kitty: Invalid UTF-8 in temp file path: {:?}", e);
                        return None;
                    }
                };
                log::debug!("Kitty: Reading image from temp file: {}", path);
                let data = match std::fs::read(path) {
                    Ok(d) => {
                        log::debug!("Kitty: Read {} bytes from temp file", d.len());
                        d
                    }
                    Err(e) => {
                        log::warn!("Kitty: Failed to read temp file '{}': {:?}", path, e);
                        return None;
                    }
                };
                // Delete the temp file after reading
                if let Err(e) = std::fs::remove_file(path) {
                    log::warn!("Kitty: Failed to delete temp file '{}': {:?}", path, e);
                } else {
                    log::debug!("Kitty: Deleted temp file: {}", path);
                }
                data
            }
            TransmissionMedium::SharedMemory => {
                // Shared memory not supported yet
                log::warn!("Kitty: Shared memory transmission not supported");
                return None;
            }
        };

        // Handle compression if present
        let decompressed = match cmd.control.compression {
            Some('z') => {
                log::debug!("Kitty: Decompressing zlib data ({} bytes)", raw_data.len());
                // zlib decompression
                let mut decoder = ZlibDecoder::new(&raw_data[..]);
                let mut decompressed = Vec::new();
                if let Err(e) = decoder.read_to_end(&mut decompressed) {
                    log::warn!("Kitty: Failed to decompress zlib data: {:?}", e);
                    return None;
                }
                log::debug!("Kitty: Decompressed to {} bytes", decompressed.len());
                decompressed
            }
            _ => raw_data,
        };

        // Decode based on format
        log::debug!("Kitty: Decoding image format {:?}", cmd.control.format);
        match cmd.control.format {
            KittyFormat::Png => self.decode_png(&decompressed),
            KittyFormat::Rgb24 => self.decode_rgb(&decompressed, &cmd.control),
            KittyFormat::Rgba32 => self.decode_rgba(&decompressed, &cmd.control),
        }
    }

    fn decode_png(&self, data: &[u8]) -> Option<KittyImage> {
        log::debug!("Kitty: Decoding PNG ({} bytes)", data.len());
        // Decode PNG using the image crate
        let img = match image::load_from_memory_with_format(data, ImageFormat::Png) {
            Ok(img) => img,
            Err(e) => {
                log::warn!("Kitty: Failed to decode PNG: {:?}", e);
                return None;
            }
        };
        let rgba = img.to_rgba8();
        let width = rgba.width() as usize;
        let height = rgba.height() as usize;
        log::debug!("Kitty: PNG decoded - dimensions={}x{}", width, height);

        Some(KittyImage {
            id: 0, // Will be assigned later
            width,
            height,
            pixel_data: rgba.into_raw(),
        })
    }

    fn decode_rgb(&self, data: &[u8], control: &KittyControlData) -> Option<KittyImage> {
        let width = match control.width {
            Some(w) => w,
            None => {
                log::warn!("Kitty: RGB decode failed - missing width");
                return None;
            }
        };
        let height = match control.height {
            Some(h) => h,
            None => {
                log::warn!("Kitty: RGB decode failed - missing height");
                return None;
            }
        };

        log::debug!(
            "Kitty: Decoding RGB24 - expected {}x{} ({} bytes), got {} bytes",
            width,
            height,
            width * height * 3,
            data.len()
        );

        if data.len() < width * height * 3 {
            log::warn!(
                "Kitty: RGB decode failed - insufficient data (need {}, got {})",
                width * height * 3,
                data.len()
            );
            return None;
        }

        // Convert RGB to RGBA
        let mut rgba = Vec::with_capacity(width * height * 4);
        for chunk in data.chunks(3) {
            if chunk.len() == 3 {
                rgba.push(chunk[0]); // R
                rgba.push(chunk[1]); // G
                rgba.push(chunk[2]); // B
                rgba.push(255); // A
            }
        }
        log::debug!("Kitty: RGB24 decoded - dimensions={}x{}", width, height);

        Some(KittyImage {
            id: 0, // Will be assigned later
            width,
            height,
            pixel_data: rgba,
        })
    }

    fn decode_rgba(&self, data: &[u8], control: &KittyControlData) -> Option<KittyImage> {
        let width = match control.width {
            Some(w) => w,
            None => {
                log::warn!("Kitty: RGBA decode failed - missing width");
                return None;
            }
        };
        let height = match control.height {
            Some(h) => h,
            None => {
                log::warn!("Kitty: RGBA decode failed - missing height");
                return None;
            }
        };

        log::debug!(
            "Kitty: Decoding RGBA32 - expected {}x{} ({} bytes), got {} bytes",
            width,
            height,
            width * height * 4,
            data.len()
        );

        if data.len() < width * height * 4 {
            log::warn!(
                "Kitty: RGBA decode failed - insufficient data (need {}, got {})",
                width * height * 4,
                data.len()
            );
            return None;
        }
        log::debug!("Kitty: RGBA32 decoded - dimensions={}x{}", width, height);

        Some(KittyImage {
            id: 0,
            width,
            height,
            pixel_data: data[..width * height * 4].to_vec(),
        })
    }

    fn create_placement(
        &mut self,
        image_id: u32,
        image_width: usize,
        image_height: usize,
        control: &KittyControlData,
        cursor_x_pixels: usize,
        cursor_y_pixels: usize,
    ) {
        log::debug!(
            "Kitty: Creating placement for image_id={}, image_size={}x{}, cursor_pos=({}, {})",
            image_id,
            image_width,
            image_height,
            cursor_x_pixels,
            cursor_y_pixels
        );

        let placement_id = control.placement_id.unwrap_or_else(|| {
            let id = self.next_placement_id;
            self.next_placement_id += 1;
            id
        });
        log::debug!("Kitty: Assigned placement_id={}", placement_id);

        // Position is always at cursor position
        // Note: control.display_x and display_y (x= and y=) are for SOURCE image cropping offset,
        // not for positioning. The image position is determined by cursor position.
        let pixel_x = cursor_x_pixels;
        let pixel_y = cursor_y_pixels as isize;
        log::debug!(
            "Kitty: Placement position - pixel_x={}, pixel_y={} (cursor position)",
            pixel_x,
            pixel_y
        );

        let character_cell_size = *self.character_cell_size.borrow();
        log::debug!("Kitty: Character cell size: {:?}", character_cell_size);

        let display_width = control.display_width.unwrap_or_else(|| {
            control
                .columns
                .and_then(|cols| character_cell_size.map(|cs| cols * cs.width))
                .unwrap_or(image_width)
        });
        let display_height = control.display_height.unwrap_or_else(|| {
            control
                .rows
                .and_then(|rows| character_cell_size.map(|cs| rows * cs.height))
                .unwrap_or(image_height)
        });
        let z_index = control.z_index.unwrap_or(0);

        log::debug!(
            "Kitty: Placement display size - width={} (w={:?}, c={:?}), height={} (h={:?}, r={:?}), z_index={}",
            display_width,
            control.display_width,
            control.columns,
            display_height,
            control.display_height,
            control.rows,
            z_index
        );

        let placement = KittyPlacement {
            image_id,
            placement_id,
            pixel_x,
            pixel_y,
            display_width,
            display_height,
            z_index,
        };

        // Remove any existing placement with the same ID
        self.placements.retain(|&(img_id, pl_id), _| {
            !(img_id == image_id && pl_id == placement_id)
        });

        self.placements.insert((image_id, placement_id), placement);
        log::debug!(
            "Kitty: Placement created - total placements now: {}",
            self.placements.len()
        );
    }

    fn handle_put(
        &mut self,
        cmd: &KittyCommand,
        cursor_x_pixels: usize,
        cursor_y_pixels: usize,
    ) -> Option<Vec<u8>> {
        log::debug!("Kitty: Handling put command for image_id={:?}", cmd.control.image_id);
        let image_id = match cmd.control.image_id {
            Some(id) => id,
            None => {
                log::warn!("Kitty: Put command missing image_id");
                return None;
            }
        };
        let (width, height) = {
            let store = self.kitty_image_store.borrow();
            match store.get_image(image_id) {
                Some(image) => {
                    log::debug!(
                        "Kitty: Found image in store - id={}, dimensions={}x{}",
                        image_id,
                        image.width,
                        image.height
                    );
                    (image.width, image.height)
                }
                None => {
                    log::warn!("Kitty: Image not found in store - id={}", image_id);
                    return None;
                }
            }
        };

        self.create_placement(
            image_id,
            width,
            height,
            &cmd.control,
            cursor_x_pixels,
            cursor_y_pixels,
        );

        if cmd.control.quiet == 0 {
            let response = format!("\x1b_Gi={};OK\x1b\\", image_id);
            log::debug!("Kitty: Sending put response OK for image_id={}", image_id);
            Some(response.into_bytes())
        } else {
            log::debug!("Kitty: Put in quiet mode, no response sent");
            None
        }
    }

    fn handle_delete(&mut self, cmd: &KittyCommand) -> Option<Vec<u8>> {
        let delete_type = cmd.control.delete_type.unwrap_or('a');
        let character_cell_size = *self.character_cell_size.borrow();
        let placements_before = self.placements.len();

        log::debug!(
            "Kitty: Handling delete command - type='{}', image_id={:?}, placement_id={:?}, placements_before={}",
            delete_type,
            cmd.control.image_id,
            cmd.control.placement_id,
            placements_before
        );

        // Delete placements based on delete type
        match delete_type {
            'a' | 'A' => {
                // Delete all placements
                log::debug!("Kitty: Deleting all placements");
                self.placements.clear();
            }
            'i' | 'I' => {
                // Delete by image ID
                if let Some(id) = cmd.control.image_id {
                    log::debug!("Kitty: Deleting placements for image_id={}", id);
                    self.placements.retain(|&(img_id, _), _| img_id != id);
                }
            }
            'p' | 'P' => {
                // Delete by placement ID
                if let (Some(img_id), Some(pl_id)) =
                    (cmd.control.image_id, cmd.control.placement_id)
                {
                    log::debug!(
                        "Kitty: Deleting specific placement - image_id={}, placement_id={}",
                        img_id,
                        pl_id
                    );
                    self.placements.remove(&(img_id, pl_id));
                }
            }
            'c' | 'C' => {
                // Delete placements intersecting cursor (specified by x= and y= in pixels)
                if let (Some(cursor_x), Some(cursor_y)) =
                    (cmd.control.display_x, cmd.control.display_y)
                {
                    log::debug!(
                        "Kitty: Deleting placements intersecting cursor at ({}, {})",
                        cursor_x,
                        cursor_y
                    );
                    self.placements.retain(|_, placement| {
                        let intersects = cursor_x >= placement.pixel_x
                            && cursor_x < placement.pixel_x + placement.display_width
                            && (cursor_y as isize) >= placement.pixel_y
                            && (cursor_y as isize)
                                < placement.pixel_y + placement.display_height as isize;
                        !intersects
                    });
                }
            }
            'x' | 'X' => {
                // Delete placements intersecting a column
                if let (Some(cell_size), Some(col)) = (character_cell_size, cmd.control.columns) {
                    let col_pixel_start = col * cell_size.width;
                    let col_pixel_end = col_pixel_start + cell_size.width;
                    log::debug!(
                        "Kitty: Deleting placements intersecting column {} (pixels {}-{})",
                        col,
                        col_pixel_start,
                        col_pixel_end
                    );
                    self.placements.retain(|_, placement| {
                        let intersects = placement.pixel_x < col_pixel_end
                            && placement.pixel_x + placement.display_width > col_pixel_start;
                        !intersects
                    });
                }
            }
            'y' | 'Y' => {
                // Delete placements intersecting a row
                if let (Some(cell_size), Some(row)) = (character_cell_size, cmd.control.rows) {
                    let row_pixel_start = (row * cell_size.height) as isize;
                    let row_pixel_end = row_pixel_start + cell_size.height as isize;
                    log::debug!(
                        "Kitty: Deleting placements intersecting row {} (pixels {}-{})",
                        row,
                        row_pixel_start,
                        row_pixel_end
                    );
                    self.placements.retain(|_, placement| {
                        let intersects = placement.pixel_y < row_pixel_end
                            && placement.pixel_y + placement.display_height as isize
                                > row_pixel_start;
                        !intersects
                    });
                }
            }
            'z' | 'Z' => {
                // Delete placements with specific z-index
                if let Some(z) = cmd.control.z_index {
                    log::debug!("Kitty: Deleting placements with z_index={}", z);
                    self.placements.retain(|_, placement| placement.z_index != z);
                }
            }
            _ => {
                log::debug!("Kitty: Unknown delete type '{}'", delete_type);
            }
        }

        log::debug!(
            "Kitty: Delete complete - placements_after={}, deleted={}",
            self.placements.len(),
            placements_before.saturating_sub(self.placements.len())
        );

        // Also delete from image store (for operations that affect stored images)
        self.kitty_image_store
            .borrow_mut()
            .delete(delete_type, cmd.control.image_id);

        None
    }

    /// Get image chunks that intersect with changed viewport regions
    pub fn changed_kitty_chunks_in_viewport(
        &self,
        changed_rects: HashMap<usize, usize>,
        scrollback_size_in_lines: usize,
        _viewport_width_in_cells: usize,
        viewport_x_offset: usize,
        viewport_y_offset: usize,
    ) -> Vec<KittyImageChunk> {
        let mut chunks = Vec::new();
        let mut seen_placements = std::collections::HashSet::new();

        if !self.placements.is_empty() {
            log::debug!(
                "Kitty: Getting viewport chunks - placements={}, changed_rects={}, scrollback={}, viewport_offset=({}, {})",
                self.placements.len(),
                changed_rects.len(),
                scrollback_size_in_lines,
                viewport_x_offset,
                viewport_y_offset
            );
        }

        let Some(character_cell_size) = *self.character_cell_size.borrow() else {
            if !self.placements.is_empty() {
                log::debug!("Kitty: No character cell size available, skipping viewport calculation");
            }
            return chunks;
        };

        for ((image_id, placement_id), placement) in &self.placements {
            // Skip if we've already added this placement
            if seen_placements.contains(&(*image_id, *placement_id)) {
                continue;
            }

            for (line_index, line_count) in &changed_rects {
                let changed_rect_pixel_height = line_count * character_cell_size.height;
                let changed_rect_top_edge =
                    ((line_index + scrollback_size_in_lines) * character_cell_size.height) as isize;
                let changed_rect_bottom_edge =
                    changed_rect_top_edge + changed_rect_pixel_height as isize;

                let placement_top_edge = placement.pixel_y;
                let placement_bottom_edge = placement.pixel_y + placement.display_height as isize;

                // Check if placement intersects with changed rect
                if (placement_top_edge >= changed_rect_top_edge
                    && placement_top_edge <= changed_rect_bottom_edge)
                    || (placement_bottom_edge >= changed_rect_top_edge
                        && placement_bottom_edge <= changed_rect_bottom_edge)
                    || (placement_bottom_edge >= changed_rect_bottom_edge
                        && placement_top_edge <= changed_rect_top_edge)
                {
                    // Mark this placement as seen so we don't add it multiple times
                    seen_placements.insert((*image_id, *placement_id));

                    // Calculate cell position for the placement
                    let cell_x_in_current_pane = placement.pixel_x / character_cell_size.width;
                    let cell_x = viewport_x_offset + cell_x_in_current_pane;

                    // Calculate the cell Y position based on the placement's pixel position
                    let placement_cell_y = if placement.pixel_y >= 0 {
                        placement.pixel_y as usize / character_cell_size.height
                    } else {
                        0
                    };
                    let cell_y = viewport_y_offset
                        + placement_cell_y.saturating_sub(scrollback_size_in_lines);

                    log::debug!(
                        "Kitty: Creating viewport chunk - image_id={}, placement_id={}, cell_pos=({}, {}), pixel_pos=({}, {}), display_size={}x{}",
                        *image_id,
                        *placement_id,
                        cell_x,
                        cell_y,
                        placement.pixel_x,
                        placement.pixel_y,
                        placement.display_width,
                        placement.display_height
                    );

                    // Always render the entire image - let the terminal handle it
                    // This avoids coordinate mapping issues between source and display dimensions
                    chunks.push(KittyImageChunk {
                        cell_x,
                        cell_y,
                        kitty_image_pixel_x: 0,
                        kitty_image_pixel_y: 0,
                        kitty_image_pixel_width: placement.display_width,
                        kitty_image_pixel_height: placement.display_height,
                        kitty_image_id: placement.image_id,
                        kitty_placement_id: placement.placement_id,
                    });

                    break; // Move to next placement
                }
            }
        }

        if !chunks.is_empty() {
            log::debug!("Kitty: Returning {} viewport chunks", chunks.len());
        }

        chunks
    }

    /// Offset all placements when scrollback changes
    pub fn offset_grid_top(&mut self) {
        if let Some(character_cell_size) = *self.character_cell_size.borrow() {
            let height_to_reduce = character_cell_size.height as isize;
            for (key, placement) in self.placements.iter_mut() {
                placement.pixel_y -= height_to_reduce;
                if placement.pixel_y + placement.display_height as isize <= 0 {
                    self.placements_to_reap.push(*key);
                }
            }
            for key in &self.placements_to_reap {
                self.placements.remove(key);
            }
            self.placements_to_reap.clear();
        }
    }

    /// Handle cell size changes
    pub fn character_cell_size_possibly_changed(&mut self) {
        if let (Some(previous_cell_size), Some(character_cell_size)) =
            (self.previous_cell_size, *self.character_cell_size.borrow())
        {
            if previous_cell_size != character_cell_size {
                for placement in self.placements.values_mut() {
                    placement.pixel_x = (placement.pixel_x / previous_cell_size.width)
                        * character_cell_size.width;
                    placement.pixel_y = (placement.pixel_y / previous_cell_size.height as isize)
                        * character_cell_size.height as isize;
                }
            }
        }
        self.previous_cell_size = *self.character_cell_size.borrow();
    }

    /// Clear all placements
    pub fn clear(&mut self) -> Option<Vec<u32>> {
        let image_ids: Vec<u32> = self
            .placements
            .drain()
            .map(|((image_id, _), _)| image_id)
            .collect();

        if !image_ids.is_empty() {
            Some(image_ids)
        } else {
            None
        }
    }

    /// Get placement coordinates in viewport for rendering
    pub fn placement_cell_coordinates_in_viewport(
        &self,
        viewport_height: usize,
        scrollback_height: usize,
    ) -> Vec<(usize, usize, usize, usize)> {
        let Some(cell_size) = *self.character_cell_size.borrow() else {
            return vec![];
        };

        self.placements
            .values()
            .map(|placement| {
                let scrollback_pixels = (scrollback_height * cell_size.height) as isize;
                let y_in_viewport = placement.pixel_y - scrollback_pixels;

                let image_y = y_in_viewport.max(0) as usize / cell_size.height;
                let image_x = placement.pixel_x / cell_size.width;

                let visible_height = if y_in_viewport < 0 {
                    (placement.display_height as isize + y_in_viewport).max(0) as usize
                } else {
                    placement.display_height
                };

                let cells_high =
                    (visible_height + cell_size.height - 1) / cell_size.height;
                let cells_wide =
                    (placement.display_width + cell_size.width - 1) / cell_size.width;

                (
                    image_y,
                    (image_y + cells_high).min(viewport_height),
                    image_x,
                    image_x + cells_wide,
                )
            })
            .collect()
    }
}
