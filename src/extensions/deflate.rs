//! Permessage-deflate extension

use std::fmt::{Display, Formatter};

use crate::extensions::uncompressed::PlainTextExt;
use crate::extensions::WebSocketExtension;
use crate::protocol::frame::coding::{Data, OpCode};
use crate::protocol::frame::Frame;
use crate::protocol::message::{IncompleteMessage, IncompleteMessageType};
use crate::protocol::MAX_MESSAGE_SIZE;
use crate::{Error, Message};
use flate2::{
    Compress, CompressError, Compression, Decompress, DecompressError, FlushCompress,
    FlushDecompress, Status,
};
use http::header::{InvalidHeaderValue, SEC_WEBSOCKET_EXTENSIONS};
use http::{HeaderValue, Request, Response};
use std::mem::replace;
use std::slice;

pub struct DeflateExt {
    enabled: bool,
    config: DeflateConfig,
    fragments: Vec<Frame>,
    inflator: Inflator,
    deflator: Deflator,
    uncompressed_extension: PlainTextExt,
}

impl Clone for DeflateExt {
    fn clone(&self) -> Self {
        DeflateExt {
            enabled: self.enabled,
            config: self.config,
            fragments: vec![],
            inflator: Inflator::new(),
            deflator: Deflator::new(self.config.compression_level),
            uncompressed_extension: PlainTextExt::new(self.config.max_message_size),
        }
    }
}

impl Default for DeflateExt {
    fn default() -> Self {
        DeflateExt::new(Default::default())
    }
}

impl DeflateExt {
    pub fn new(config: DeflateConfig) -> DeflateExt {
        DeflateExt {
            enabled: false,
            config,
            fragments: vec![],
            inflator: Inflator::new(),
            deflator: Deflator::new(Compression::fast()),
            uncompressed_extension: PlainTextExt::new(config.max_message_size),
        }
    }

    fn complete_message(&self, data: Vec<u8>, opcode: OpCode) -> Result<Message, Error> {
        let message_type = match opcode {
            OpCode::Data(Data::Text) => IncompleteMessageType::Text,
            OpCode::Data(Data::Binary) => IncompleteMessageType::Binary,
            _ => panic!("Bug: message is not text nor binary"),
        };

        let mut incomplete_message = IncompleteMessage::new(message_type);
        incomplete_message.extend(data, self.config.max_message_size)?;
        incomplete_message.complete()
    }

    fn parse_window_parameter<'a>(
        &self,
        mut param_iter: impl Iterator<Item = &'a str>,
    ) -> Result<Option<u8>, String> {
        if let Some(window_bits_str) = param_iter.next() {
            match window_bits_str.trim().parse() {
                Ok(mut window_bits) => {
                    if window_bits == 8 {
                        window_bits = 9;
                    }

                    if window_bits >= 9 && window_bits <= 15 {
                        if window_bits as u8 != self.config.max_window_bits {
                            Ok(Some(window_bits))
                        } else {
                            Ok(None)
                        }
                    } else {
                        Err(format!(
                            "Invalid server_max_window_bits parameter: {}",
                            window_bits
                        ))
                    }
                }
                Err(e) => Err(e.to_string()),
            }
        } else {
            Ok(None)
        }
    }

    fn decline<T>(&mut self, res: &mut Response<T>) {
        self.enabled = false;
        res.headers_mut().remove(EXT_NAME);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DeflateConfig {
    pub max_message_size: Option<usize>,
    pub max_window_bits: u8,
    pub request_no_context_takeover: bool,
    pub accept_no_context_takeover: bool,
    pub fragments_capacity: usize,
    pub fragments_grow: bool,
    pub compress_reset: bool,
    pub decompress_reset: bool,
    pub compression_level: Compression,
}

impl DeflateConfig {
    pub fn with_compression_level(compression_level: Compression) -> DeflateConfig {
        DeflateConfig {
            compression_level,
            ..Default::default()
        }
    }
}

impl Default for DeflateConfig {
    fn default() -> Self {
        DeflateConfig {
            max_message_size: Some(MAX_MESSAGE_SIZE),
            max_window_bits: 15,
            request_no_context_takeover: false,
            accept_no_context_takeover: true,
            fragments_capacity: 10,
            fragments_grow: true,
            compress_reset: false,
            decompress_reset: false,
            compression_level: Compression::best(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum DeflateExtensionError {
    DeflateError(String),
    InflateError(String),
    NegotiationError(String),
}

impl Display for DeflateExtensionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DeflateExtensionError::DeflateError(m) => write!(f, "{}", m),
            DeflateExtensionError::InflateError(m) => write!(f, "{}", m),
            DeflateExtensionError::NegotiationError(m) => write!(f, "{}", m),
        }
    }
}

impl std::error::Error for DeflateExtensionError {}

impl From<DeflateExtensionError> for crate::Error {
    fn from(e: DeflateExtensionError) -> Self {
        crate::Error::ExtensionError(Box::new(e))
    }
}

impl From<InvalidHeaderValue> for DeflateExtensionError {
    fn from(e: InvalidHeaderValue) -> Self {
        DeflateExtensionError::NegotiationError(e.to_string())
    }
}

const EXT_NAME: &str = "permessage-deflate";

impl WebSocketExtension for DeflateExt {
    type Error = DeflateExtensionError;

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn rsv1(&self) -> bool {
        if self.enabled {
            true
        } else {
            self.uncompressed_extension.rsv1()
        }
    }

    fn on_make_request<T>(&mut self, mut request: Request<T>) -> Request<T> {
        let mut header_value = String::from(EXT_NAME);
        let DeflateConfig {
            max_window_bits,
            request_no_context_takeover,
            ..
        } = self.config;

        if max_window_bits < 15 {
            header_value.push_str(&format!(
                "; client_max_window_bits={}; server_max_window_bits={}",
                max_window_bits, max_window_bits
            ))
        } else {
            header_value.push_str("; client_max_window_bits")
        }

        if request_no_context_takeover {
            header_value.push_str("; server_no_context_takeover")
        }

        request.headers_mut().append(
            SEC_WEBSOCKET_EXTENSIONS,
            HeaderValue::from_str(&header_value).unwrap(),
        );

        request
    }

    fn on_receive_request<T>(
        &mut self,
        request: &Request<T>,
        response: &mut Response<T>,
    ) -> Result<(), Self::Error> {
        for header in request.headers().get_all(SEC_WEBSOCKET_EXTENSIONS) {
            match header.to_str() {
                Ok(header) => {
                    let mut res_ext = String::with_capacity(header.len());
                    let mut s_takeover = false;
                    let mut c_takeover = false;
                    let mut s_max = false;
                    let mut c_max = false;

                    for param in header.split(';') {
                        match param.trim() {
                            "permessage-deflate" => res_ext.push_str("permessage-deflate"),
                            "server_no_context_takeover" => {
                                if s_takeover {
                                    self.decline(response);
                                } else {
                                    s_takeover = true;
                                    if self.config.accept_no_context_takeover {
                                        self.config.compress_reset = true;
                                        res_ext.push_str("; server_no_context_takeover");
                                    }
                                }
                            }
                            "client_no_context_takeover" => {
                                if c_takeover {
                                    self.decline(response);
                                } else {
                                    c_takeover = true;
                                    self.config.decompress_reset = true;
                                    res_ext.push_str("; client_no_context_takeover");
                                }
                            }
                            param if param.starts_with("server_max_window_bits") => {
                                if s_max {
                                    self.decline(response);
                                } else {
                                    s_max = true;
                                    let mut param_iter = param.split('=');
                                    param_iter.next(); // we already know the name
                                    if let Some(window_bits_str) = param_iter.next() {
                                        if let Ok(window_bits) = window_bits_str.trim().parse() {
                                            if window_bits >= 9 && window_bits <= 15 {
                                                if window_bits < self.config.max_window_bits {
                                                    self.deflator = Deflator {
                                                        compress: Compress::new_with_window_bits(
                                                            self.config.compression_level,
                                                            false,
                                                            window_bits,
                                                        ),
                                                    };
                                                    res_ext.push_str("; ");
                                                    res_ext.push_str(param)
                                                }
                                            } else {
                                                self.decline(response);
                                            }
                                        } else {
                                            self.decline(response);
                                        }
                                    }
                                }
                            }
                            param if param.starts_with("client_max_window_bits") => {
                                if c_max {
                                    self.decline(response);
                                } else {
                                    c_max = true;
                                    let mut param_iter = param.split('=');
                                    param_iter.next(); // we already know the name
                                    if let Some(window_bits_str) = param_iter.next() {
                                        if let Ok(window_bits) = window_bits_str.trim().parse() {
                                            if window_bits >= 9 && window_bits <= 15 {
                                                if window_bits < self.config.max_window_bits {
                                                    self.inflator = Inflator {
                                                        decompress:
                                                            Decompress::new_with_window_bits(
                                                                false,
                                                                window_bits,
                                                            ),
                                                    };
                                                    res_ext.push_str("; ");
                                                    res_ext.push_str(param);
                                                    continue;
                                                }
                                            } else {
                                                self.decline(response);
                                            }
                                        } else {
                                            self.decline(response);
                                        }
                                    }
                                    res_ext.push_str("; ");
                                    res_ext.push_str(&format!(
                                        "client_max_window_bits={}",
                                        self.config.max_window_bits
                                    ))
                                }
                            }
                            _ => {
                                // decline all extension offers because we got a bad parameter
                                self.decline(response);
                            }
                        }
                    }

                    if !res_ext.contains("client_no_context_takeover")
                        && self.config.request_no_context_takeover
                    {
                        self.config.decompress_reset = true;
                        res_ext.push_str("; client_no_context_takeover");
                    }

                    if !res_ext.contains("server_max_window_bits") {
                        res_ext.push_str("; ");
                        res_ext.push_str(&format!(
                            "server_max_window_bits={}",
                            self.config.max_window_bits
                        ))
                    }

                    if !res_ext.contains("client_max_window_bits")
                        && self.config.max_window_bits < 15
                    {
                        continue;
                    }

                    response
                        .headers_mut()
                        .insert(SEC_WEBSOCKET_EXTENSIONS, HeaderValue::from_str(&res_ext)?);

                    self.enabled = true;

                    return Ok(());
                }
                Err(e) => {
                    self.enabled = false;
                    return Err(DeflateExtensionError::NegotiationError(format!(
                        "Failed to parse header: {}",
                        e,
                    )));
                }
            }
        }

        self.decline(response);
        Ok(())
    }

    fn on_response<T>(&mut self, response: &Response<T>) -> Result<(), Self::Error> {
        let mut extension_name = false;
        let mut server_takeover = false;
        let mut client_takeover = false;
        let mut server_max_window_bits = false;
        let mut client_max_window_bits = false;

        for header in response.headers().get_all(SEC_WEBSOCKET_EXTENSIONS) {
            match header.to_str() {
                Ok(header) => {
                    for param in header.split(';') {
                        match param.trim() {
                            "permessage-deflate" => {
                                if extension_name {
                                    return Err(DeflateExtensionError::NegotiationError(format!(
                                        "Duplicate extension name permessage-deflate"
                                    )));
                                } else {
                                    self.enabled = true;
                                    extension_name = true;
                                }
                            }
                            "server_no_context_takeover" => {
                                if server_takeover {
                                    return Err(DeflateExtensionError::NegotiationError(format!(
                                        "Duplicate extension parameter server_no_context_takeover"
                                    )));
                                } else {
                                    server_takeover = true;
                                    self.config.decompress_reset = true;
                                }
                            }
                            "client_no_context_takeover" => {
                                if client_takeover {
                                    return Err(DeflateExtensionError::NegotiationError(format!(
                                        "Duplicate extension parameter client_no_context_takeover"
                                    )));
                                } else {
                                    client_takeover = true;

                                    if self.config.accept_no_context_takeover {
                                        self.config.compress_reset = true;
                                    } else {
                                        return Err(DeflateExtensionError::NegotiationError(
                                            format!("The client requires context takeover."),
                                        ));
                                    }
                                }
                            }
                            param if param.starts_with("server_max_window_bits") => {
                                if server_max_window_bits {
                                    return Err(DeflateExtensionError::NegotiationError(format!(
                                        "Duplicate extension parameter server_max_window_bits"
                                    )));
                                } else {
                                    server_max_window_bits = true;

                                    match self.parse_window_parameter(param.split("=").skip(1)) {
                                        Ok(Some(bits)) => {
                                            self.deflator = Deflator {
                                                compress: Compress::new_with_window_bits(
                                                    self.config.compression_level,
                                                    false,
                                                    bits,
                                                ),
                                            };
                                        }
                                        Ok(None) => {}
                                        Err(e) => {
                                            return Err(DeflateExtensionError::NegotiationError(
                                                format!(
                                                    "server_max_window_bits parameter error: {}",
                                                    e
                                                ),
                                            ))
                                        }
                                    }
                                }
                            }
                            param if param.starts_with("client_max_window_bits") => {
                                if client_max_window_bits {
                                    return Err(DeflateExtensionError::NegotiationError(format!(
                                        "Duplicate extension parameter client_max_window_bits"
                                    )));
                                } else {
                                    client_max_window_bits = true;

                                    match self.parse_window_parameter(param.split("=").skip(1)) {
                                        Ok(Some(bits)) => {
                                            self.inflator = Inflator {
                                                decompress: Decompress::new_with_window_bits(
                                                    false, bits,
                                                ),
                                            };
                                        }
                                        Ok(None) => {}
                                        Err(e) => {
                                            return Err(DeflateExtensionError::NegotiationError(
                                                format!(
                                                    "client_max_window_bits parameter error: {}",
                                                    e
                                                ),
                                            ))
                                        }
                                    }
                                }
                            }
                            param => {
                                return Err(DeflateExtensionError::NegotiationError(format!(
                                    "Unknown permessage-deflate parameter: {}",
                                    param
                                )));
                            }
                        }
                    }
                }
                Err(e) => {
                    self.enabled = false;

                    return Err(DeflateExtensionError::NegotiationError(format!(
                        "Failed to parse extension parameter: {}",
                        e
                    )));
                }
            }
        }

        Ok(())
    }

    fn on_send_frame(&mut self, mut frame: Frame) -> Result<Frame, Self::Error> {
        if self.enabled {
            if let OpCode::Data(_) = frame.header().opcode {
                let mut compressed = Vec::with_capacity(frame.payload().len());
                self.deflator.compress(frame.payload(), &mut compressed)?;

                let len = compressed.len();
                compressed.truncate(len - 4);

                *frame.payload_mut() = compressed;
                frame.header_mut().rsv1 = true;

                if self.config.compress_reset {
                    self.deflator.reset();
                }
            }
        }

        Ok(frame)
    }

    fn on_receive_frame(&mut self, mut frame: Frame) -> Result<Option<Message>, Self::Error> {
        match frame.header().opcode {
            OpCode::Control(_) => unreachable!(),
            _ => {
                if self.enabled && (!self.fragments.is_empty() || frame.header().rsv1) {
                    if !frame.header().is_final {
                        self.fragments.push(frame);
                        return Ok(None);
                    } else {
                        let message = if let OpCode::Data(Data::Continue) = frame.header().opcode {
                            if !self.config.fragments_grow
                                && self.config.fragments_capacity == self.fragments.len()
                            {
                                return Err(DeflateExtensionError::DeflateError(
                                    "Exceeded max fragments.".into(),
                                ));
                            } else {
                                self.fragments.push(frame);
                            }

                            let opcode = self.fragments.first().unwrap().header().opcode;
                            let size = self
                                .fragments
                                .iter()
                                .fold(0, |len, frame| len + frame.payload().len());
                            let mut compressed = Vec::with_capacity(size);
                            let mut decompressed = Vec::with_capacity(size * 2);

                            replace(
                                &mut self.fragments,
                                Vec::with_capacity(self.config.fragments_capacity),
                            )
                            .into_iter()
                            .for_each(|f| {
                                compressed.extend(f.into_data());
                            });

                            compressed.extend(&[0, 0, 255, 255]);

                            self.inflator.decompress(&compressed, &mut decompressed)?;

                            self.complete_message(decompressed, opcode)
                        } else {
                            frame.payload_mut().extend(&[0, 0, 255, 255]);

                            let mut decompress_output =
                                Vec::with_capacity(frame.payload().len() * 2);
                            self.inflator
                                .decompress(frame.payload(), &mut decompress_output)?;

                            self.complete_message(decompress_output, frame.header().opcode)
                        };

                        if self.config.decompress_reset {
                            self.inflator.reset(false);
                        }

                        match message {
                            Ok(message) => Ok(Some(message)),
                            Err(e) => Err(DeflateExtensionError::DeflateError(e.to_string())),
                        }
                    }
                } else {
                    self.uncompressed_extension
                        .on_receive_frame(frame)
                        .map_err(|e| DeflateExtensionError::DeflateError(e.to_string()))
                }
            }
        }
    }
}

impl From<DecompressError> for DeflateExtensionError {
    fn from(e: DecompressError) -> Self {
        DeflateExtensionError::InflateError(e.to_string())
    }
}

impl From<CompressError> for DeflateExtensionError {
    fn from(e: CompressError) -> Self {
        DeflateExtensionError::DeflateError(e.to_string())
    }
}

struct Deflator {
    compress: Compress,
}

impl Deflator {
    pub fn new(compresion: Compression) -> Deflator {
        Deflator {
            compress: Compress::new(compresion, false),
        }
    }

    fn reset(&mut self) {
        self.compress.reset()
    }

    pub fn compress(&mut self, input: &[u8], output: &mut Vec<u8>) -> Result<(), CompressError> {
        let mut read_buff = Vec::from(input);
        let mut output_size;

        loop {
            output_size = output.len();

            if output_size == output.capacity() {
                output.reserve(input.len());
            }

            let before_out = self.compress.total_out();
            let before_in = self.compress.total_in();

            let status = self
                .compress
                .compress_vec(&read_buff, output, FlushCompress::Sync)?;

            let consumed = (self.compress.total_in() - before_in) as usize;
            read_buff = read_buff.split_off(consumed);

            unsafe {
                output.set_len((self.compress.total_out() - before_out) as usize + output_size);
            }

            match status {
                Status::Ok | Status::BufError => {
                    if before_out == self.compress.total_out() && read_buff.is_empty() {
                        return Ok(());
                    }
                }
                s => panic!(s),
            }
        }
    }
}

struct Inflator {
    decompress: Decompress,
}

impl Inflator {
    pub fn new() -> Inflator {
        Inflator {
            decompress: Decompress::new(false),
        }
    }

    fn reset(&mut self, zlib_header: bool) {
        self.decompress.reset(zlib_header)
    }

    pub fn decompress(
        &mut self,
        input: &[u8],
        output: &mut Vec<u8>,
    ) -> Result<(), DecompressError> {
        let mut read_buff = Vec::from(input);
        let mut output_size;

        loop {
            output_size = output.len();

            if output_size == output.capacity() {
                output.reserve(input.len());
            }

            let before_out = self.decompress.total_out();
            let before_in = self.decompress.total_in();

            let out_slice = unsafe {
                slice::from_raw_parts_mut(
                    output.as_mut_ptr().offset(output_size as isize),
                    output.capacity() - output_size,
                )
            };

            let status =
                self.decompress
                    .decompress(&read_buff, out_slice, FlushDecompress::Sync)?;

            let consumed = (self.decompress.total_in() - before_in) as usize;
            read_buff = read_buff.split_off(consumed);

            unsafe {
                output.set_len((self.decompress.total_out() - before_out) as usize + output_size);
            }

            match status {
                Status::Ok | Status::BufError => {
                    if before_out == self.decompress.total_out() && read_buff.is_empty() {
                        return Ok(());
                    }
                }
                s => panic!(s),
            }
        }
    }
}
