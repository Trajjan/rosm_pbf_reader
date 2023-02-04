//! A low-level library for parsing OSM data in PBF format.
//!
//! An OSM PBF file is a sequence of blobs. These blobs can be read with [`read_blob`]. The
//! [`RawBlock`]s returned by `read_blob` can then be decompressed and parsed by
//! [`BlockParser::parse_block`], which returns a [`Block`], containing either a parsed
//! header/primitive block or an unknown block's binary data.
//!
//! The library also provides utilities for reading densely or delta encoded data in these blocks.
//!
//! Raw header and primitive block definitions (generated by `quick-protobuf`) are exported
//! through the `pbf` module.
//!
//! # Links
//!
//! - [OSM PBF format documentation](https://wiki.openstreetmap.org/wiki/PBF_Format)

#[cfg(feature = "default")]
use flate2::read::ZlibDecoder;

use prost::Message;

use std::convert::From;
#[cfg(feature = "default")]
use std::io::prelude::*;
use std::io::ErrorKind;
use std::iter::{Enumerate, Zip};
use std::ops::AddAssign;
use std::slice::{ChunksExact, Iter};
use std::str;
use std::str::Utf8Error;

pub mod pbf;
pub mod util;

/// Possible errors returned by the library.
#[derive(Debug)]
pub enum Error {
    /// Returned when a PBF parse error has occured.
    PbfParseError(prost::DecodeError),
    /// Returned when reading from the input stream or decompression of blob data has failed.
    IoError(std::io::Error),
    /// Returned when a blob header with an invalid size (negative or >=64 KB) is encountered.
    InvalidBlobHeader,
    /// Returned when blob data with an invalid size (negative or >=32 MB) is encountered.
    InvalidBlobData,
    /// Returned when an error has occured during blob decompression.
    DecompressionError(DecompressionError),
    /// Returned when some assumption in the data is violated (for example, an out of bounds index is encountered).
    LogicError(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for Error {}

/// Result of [`BlockParser::parse_block`].
pub enum Block<'a> {
    /// A raw `OSMHeader` block.
    Header(pbf::HeaderBlock),
    /// A raw `OSMData` (primitive) block.
    Primitive(pbf::PrimitiveBlock),
    /// An unknown block.
    Unknown(&'a [u8]),
}

enum BlockType {
    Header,
    Primitive,
    Unknown,
}

impl From<&str> for BlockType {
    fn from(value: &str) -> Self {
        match value {
            "OSMHeader" => BlockType::Header,
            "OSMData" => BlockType::Primitive,
            _ => BlockType::Unknown,
        }
    }
}

/// An unparsed, possibly compressed block.
pub struct RawBlock {
    r#type: BlockType,
    data: Vec<u8>,
}

/// Reads the next blob from `pbf`.
///
/// # Examples
///
/// ```no_run
/// use rosm_pbf_reader::read_blob;
///
/// use std::fs::File;
///
/// let mut file = File::open("some.osm.pbf").unwrap();
///
/// while let Some(result) = read_blob(&mut file) {
///     match result {
///         Ok(raw_block) => {}
///         Err(error) => {}
///     }
/// }
/// ```
pub fn read_blob<Input>(pbf: &mut Input) -> Option<Result<RawBlock, Error>>
where
    Input: std::io::Read,
{
    use pbf::BlobHeader;

    let mut header_size_buffer = [0u8; 4];

    if let Err(error) = pbf.read_exact(&mut header_size_buffer) {
        return match error.kind() {
            ErrorKind::UnexpectedEof => None,
            _ => Some(Err(Error::IoError(error))),
        };
    }

    let blob_header_size = i32::from_be_bytes(header_size_buffer);

    if !(0..64 * 1024).contains(&blob_header_size) {
        return Some(Err(Error::InvalidBlobHeader));
    }

    let mut blob = vec![0u8; blob_header_size as usize];
    if let Err(error) = pbf.read_exact(&mut blob) {
        return Some(Err(Error::IoError(error)));
    }

    let blob_header = match BlobHeader::decode(&*blob) {
        Ok(blob_header) => blob_header,
        Err(error) => return Some(Err(Error::PbfParseError(error))),
    };

    let block_type = BlockType::from(blob_header.r#type.as_ref());
    let blob_size = blob_header.datasize;

    if !(0..32 * 1024 * 1024).contains(&blob_size) {
        return Some(Err(Error::InvalidBlobData));
    }

    blob.resize_with(blob_size as usize, Default::default);

    if let Err(error) = pbf.read_exact(&mut blob) {
        return Some(Err(Error::IoError(error)));
    }

    let raw_block = RawBlock {
        r#type: block_type,
        data: blob,
    };

    Some(Ok(raw_block))
}

/// Blob compression method.
pub enum CompressionMethod {
    /// LZ4
    Lz4,
    /// LZMA
    Lzma,
    /// ZLib
    Zlib,
    /// Zstandard
    Zstd,
}

/// Possible errors returned by [Decompressor] implementations.
#[derive(Debug)]
pub enum DecompressionError {
    /// The given compression method isn't supported by the decompressor.
    UnsupportedCompression,
    /// An internal error occured during decompression.
    InternalError(Box<dyn std::error::Error + Send + Sync>),
}

/// Trait for custom decompression support.
pub trait Decompressor {
    /// Decompresses `input` blob into the preallocated `output` slice.
    fn decompress(method: CompressionMethod, input: &[u8], output: &mut [u8]) -> Result<(), DecompressionError>;
}

/// The default blob decompressor.
///
/// Supports ZLib decompression if default features are enabled.
pub struct DefaultDecompressor;

impl Decompressor for DefaultDecompressor {
    #[cfg(feature = "default")]
    fn decompress(method: CompressionMethod, input: &[u8], output: &mut [u8]) -> Result<(), DecompressionError> {
        match method {
            CompressionMethod::Zlib => {
                let mut decoder = ZlibDecoder::new(input.as_ref());

                match decoder.read_exact(output) {
                    Ok(_) => Ok(()),
                    Err(error) => Err(DecompressionError::InternalError(Box::new(error))),
                }
            }
            _ => Err(DecompressionError::UnsupportedCompression),
        }
    }

    #[cfg(not(feature = "default"))]
    fn decompress(_method: CompressionMethod, _input: &[u8], _output: &mut [u8]) -> Result<(), DecompressionError> {
        Err(DecompressionError::UnsupportedCompression)
    }
}

/// Parser with an internal buffer for `RawBlock`s.
///
/// When multiple threads are used to speed up parsing, it's recommended to use a single
/// `BlockParser` per thread (e.g. by making it thread local), so its internal buffer remains
/// alive, avoiding repeated memory allocations.
pub struct BlockParser<D: Decompressor = DefaultDecompressor> {
    block_buffer: Vec<u8>,
    decompressor: std::marker::PhantomData<D>,
}

impl Default for BlockParser {
    fn default() -> Self {
        BlockParser::<DefaultDecompressor>::new()
    }
}

impl<D: Decompressor> BlockParser<D> {
    /// Creates a new `BlockParser`.
    pub fn new() -> Self {
        Self {
            block_buffer: Vec::new(),
            decompressor: Default::default(),
        }
    }

    /// Parses `raw_block` into a header, primitive or unknown block.
    #[allow(deprecated)]
    pub fn parse_block(&mut self, raw_block: RawBlock) -> Result<Block, Error> {
        let blob = match pbf::Blob::decode(&*raw_block.data) {
            Ok(blob) => blob,
            Err(error) => return Err(Error::PbfParseError(error)),
        };

        if let Some(uncompressed_size) = blob.raw_size {
            self.block_buffer
                .resize_with(uncompressed_size as usize, Default::default);
        }

        if let Some(blob_data) = blob.data {
            match blob_data {
                pbf::blob::Data::Raw(raw_data) => self.block_buffer.extend_from_slice(&raw_data),
                pbf::blob::Data::ZlibData(zlib_data) => {
                    if let Err(error) = D::decompress(CompressionMethod::Zlib, &zlib_data, &mut self.block_buffer) {
                        return Err(Error::DecompressionError(error));
                    }
                }
                pbf::blob::Data::Lz4Data(lz4_data) => {
                    if let Err(error) = D::decompress(CompressionMethod::Lz4, &lz4_data, &mut self.block_buffer) {
                        return Err(Error::DecompressionError(error));
                    }
                }
                pbf::blob::Data::LzmaData(lzma_data) => {
                    if let Err(error) = D::decompress(CompressionMethod::Lzma, &lzma_data, &mut self.block_buffer) {
                        return Err(Error::DecompressionError(error));
                    }
                }
                pbf::blob::Data::ZstdData(zstd_data) => {
                    if let Err(error) = D::decompress(CompressionMethod::Zstd, &zstd_data, &mut self.block_buffer) {
                        return Err(Error::DecompressionError(error));
                    }
                }
                pbf::blob::Data::ObsoleteBzip2Data(_) => return Err(Error::InvalidBlobData),
            }
        } else {
            return Err(Error::InvalidBlobData);
        }

        match raw_block.r#type {
            BlockType::Header => match pbf::HeaderBlock::decode(&*self.block_buffer) {
                Ok(header_block) => Ok(Block::Header(header_block)),
                Err(error) => Err(Error::PbfParseError(error)),
            },
            BlockType::Primitive => match pbf::PrimitiveBlock::decode(&*self.block_buffer) {
                Ok(primitive_block) => Ok(Block::Primitive(primitive_block)),
                Err(error) => Err(Error::PbfParseError(error)),
            },
            BlockType::Unknown => Ok(Block::Unknown(&self.block_buffer)),
        }
    }
}

/// Utility for reading tags of dense nodes.
///
/// See [`DenseNode::key_value_indices`].
pub struct DenseTagReader<'a> {
    string_table: &'a pbf::StringTable,

    /// Iterator over [key_index, value_index] slices
    indices_it: ChunksExact<'a, i32>,
}

impl<'a> DenseTagReader<'a> {
    pub fn new(string_table: &'a pbf::StringTable, key_value_indices: &'a [i32]) -> Self {
        Self {
            string_table,
            indices_it: key_value_indices.chunks_exact(2),
        }
    }
}

impl<'a> Iterator for DenseTagReader<'a> {
    /// (tag key, tag value) pair
    type Item = (Result<&'a str, Error>, Result<&'a str, Error>);

    fn next(&mut self) -> Option<Self::Item> {
        match self.indices_it.next() {
            Some(indices) => {
                let decode_string = |index: i32| -> Result<&str, Error> {
                    if let Ok(index) = TryInto::<usize>::try_into(index) {
                        if let Some(bytes) = self.string_table.s.get(index) {
                            if let Ok(utf8_string) = str::from_utf8(bytes) {
                                Ok(utf8_string)
                            } else {
                                Err(Error::LogicError(format!(
                                    "string at index {} is not valid UTF-8",
                                    index
                                )))
                            }
                        } else {
                            Err(Error::LogicError(format!(
                                "string table index {} is out of bounds ({})",
                                index,
                                self.string_table.s.len()
                            )))
                        }
                    } else {
                        Err(Error::LogicError(format!("string table index {} is invalid", index)))
                    }
                };

                let key = decode_string(indices[0]);
                let value = decode_string(indices[1]);

                Some((key, value))
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod dense_tag_reader_tests {
    use super::*;

    #[test]
    fn valid_input() {
        let key_vals = ["", "key1", "val1", "key2", "val2"];
        let mut string_table = pbf::StringTable::default();
        string_table.s = key_vals.iter().map(|s| s.as_bytes().to_vec()).collect();

        let key_value_indices = [1, 2];
        let mut reader = DenseTagReader::new(&string_table, &key_value_indices);

        match reader.next() {
            Some(tags) => match tags {
                (Ok("key1"), Ok("val1")) => {}
                _ => assert!(false),
            },
            None => assert!(false),
        }
        assert!(reader.next().is_none());
    }
}

/// Utility for reading tags.
pub struct TagReader<'a> {
    string_table: &'a pbf::StringTable,
    key_indices: &'a [u32],
    value_indices: &'a [u32],
    idx: usize,
}

impl<'a> TagReader<'a> {
    /// Constructs a new `TagReader` from key and value index slices, and a corresponding string table.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rosm_pbf_reader::{pbf, TagReader};
    ///
    /// fn process_primitive_block(block: pbf::PrimitiveBlock) {
    ///     for group in &block.primitivegroup {
    ///         for way in &group.ways {
    ///             let tags = TagReader::new(&way.keys, &way.vals, &block.stringtable);
    ///             for (key, value) in tags {
    ///                 println!("{}: {}", key.unwrap(), value.unwrap());
    ///             }
    ///         }
    ///     }
    /// }
    pub fn new(key_indices: &'a [u32], value_indices: &'a [u32], string_table: &'a pbf::StringTable) -> Self {
        TagReader {
            string_table,
            key_indices,
            value_indices,
            idx: 0,
        }
    }
}

impl<'a> Iterator for TagReader<'a> {
    type Item = (Result<&'a str, Utf8Error>, Result<&'a str, Utf8Error>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx < self.key_indices.len() {
            let key = str::from_utf8(self.string_table.s[self.key_indices[self.idx] as usize].as_ref());
            let value = str::from_utf8(self.string_table.s[self.value_indices[self.idx] as usize].as_ref());

            self.idx += 1;

            Some((key, value))
        } else {
            None
        }
    }
}

/// An unpacked dense node, returned when iterating on [`DenseNodeReader`].
pub struct DenseNode<'a> {
    pub id: i64,

    /// Latitude of the node in an encoded format.
    /// Use [`util::normalize_coord`] to convert it to nanodegrees.
    pub lat: i64,

    /// Longitude of the node in an encoded format.
    /// Use [`util::normalize_coord`] to convert it to nanodegrees.
    pub lon: i64,

    /// Optional metadata.
    pub info: Option<pbf::Info>,

    /// Key/value index slice of [`pbf::DenseNodes::keys_vals`]. Indices point into a [`pbf::StringTable`].
    /// Use [`DenseTagReader`] to read these key/value pairs conveniently.
    pub key_value_indices: &'a [i32],
}

#[derive(Default)]
struct DeltaCodedValues {
    id: i64,
    lat: i64,
    lon: i64,
    timestamp: i64,
    changeset: i64,
    uid: i32,
    user_sid: u32,
}

/// Utility for reading delta-encoded dense nodes.
pub struct DenseNodeReader<'a> {
    data: &'a pbf::DenseNodes,
    data_it: Enumerate<Zip<Iter<'a, i64>, Zip<Iter<'a, i64>, Iter<'a, i64>>>>, // (data_idx, (id_delta, (lat_delta, lon_delta))) iterator
    key_value_idx: usize,      // Starting index of the next node's keys/values
    current: DeltaCodedValues, // Current values of delta coded fields
}

impl<'a> DenseNodeReader<'a> {
    /// Constructs a new `DenseNodeReader` from a slice of nodes.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rosm_pbf_reader::{pbf, DenseNodeReader, DenseTagReader, Error};
    ///
    /// fn process_primitive_block(block: pbf::PrimitiveBlock) -> Result<(), Error> {
    ///     for group in &block.primitivegroup {
    ///         if let Some(dense_nodes) = &group.dense {
    ///             let nodes = DenseNodeReader::new(&dense_nodes)?;
    ///             for node in nodes {
    ///                 let tags = DenseTagReader::new(&block.stringtable, node?.key_value_indices);
    ///                 for (key, value) in tags {
    ///                     println!("{}: {}", key?, value?);
    ///                 }
    ///             }
    ///         }
    ///     }
    ///
    ///     Ok(())
    /// }
    /// ```
    pub fn new(data: &'a pbf::DenseNodes) -> Result<Self, Error> {
        if data.lat.len() != data.id.len() || data.lon.len() != data.id.len() {
            Err(Error::LogicError(format!(
                "dense node id/lat/lon counts differ: {}/{}/{}",
                data.id.len(),
                data.lat.len(),
                data.lon.len()
            )))
        } else {
            let data_it = data.id.iter().zip(data.lat.iter().zip(data.lon.iter())).enumerate();

            Ok(DenseNodeReader {
                data,
                data_it,
                key_value_idx: 0,
                current: DeltaCodedValues::default(),
            })
        }
    }
}

fn delta_decode<T>(current: &mut T, delta: Option<&T>) -> Option<T>
where
    T: AddAssign<T> + Copy,
{
    match delta {
        Some(delta) => {
            *current += *delta;
            Some(*current)
        }
        None => None,
    }
}

impl<'a> Iterator for DenseNodeReader<'a> {
    type Item = Result<DenseNode<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((data_idx, (id_delta, (lat_delta, lon_delta)))) = self.data_it.next() {
            self.current.id += id_delta;
            self.current.lat += lat_delta;
            self.current.lon += lon_delta;

            let info = match &self.data.denseinfo {
                Some(dense_info) => {
                    let user_sid = match dense_info.user_sid.get(data_idx) {
                        Some(user_sid_delta) => {
                            if let Some(current_user_sid) = self.current.user_sid.checked_add_signed(*user_sid_delta) {
                                self.current.user_sid = current_user_sid;
                                Some(self.current.user_sid)
                            } else {
                                return Some(Err(Error::LogicError(format!(
                                    "delta decoding `user_sid` results in a negative integer: {}+{}",
                                    self.current.user_sid, user_sid_delta
                                ))));
                            }
                        }
                        None => None,
                    };

                    Some(pbf::Info {
                        version: dense_info.version.get(data_idx).cloned(),
                        timestamp: delta_decode(&mut self.current.timestamp, dense_info.timestamp.get(data_idx)),
                        changeset: delta_decode(&mut self.current.changeset, dense_info.changeset.get(data_idx)),
                        uid: delta_decode(&mut self.current.uid, dense_info.uid.get(data_idx)),
                        user_sid,
                        visible: dense_info.visible.get(data_idx).cloned(),
                    })
                }
                None => None,
            };

            let key_value_indices = if !self.data.keys_vals.is_empty() {
                let next_zero = &self.data.keys_vals[self.key_value_idx..]
                    .iter()
                    .enumerate()
                    .step_by(2)
                    .find(|(_, string_idx)| **string_idx == 0);

                let next_zero_idx = if let Some((next_zero_idx, _)) = next_zero {
                    self.key_value_idx + *next_zero_idx
                } else {
                    self.data.keys_vals.len()
                };

                let key_value_start = self.key_value_idx;
                self.key_value_idx = next_zero_idx + 1;

                &self.data.keys_vals[key_value_start..self.key_value_idx - 1]
            } else {
                &[]
            };

            Some(Ok(DenseNode {
                id: self.current.id,
                lat: self.current.lat,
                lon: self.current.lon,
                key_value_indices,
                info,
            }))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod dense_node_reader_tests {
    use super::*;

    #[test]
    fn valid_input() {
        let dense_info = pbf::DenseInfo {
            user_sid: vec![i32::MAX, 1],
            version: vec![2, 4],
            timestamp: vec![2, 1],
            changeset: vec![2, -1],
            uid: vec![5, -1],
            visible: vec![true, false],
        };

        let dense_nodes = pbf::DenseNodes {
            id: vec![2, -1],
            denseinfo: Some(dense_info),
            lat: vec![-3, 1],
            lon: vec![3, -1],
            keys_vals: vec![1, 2, 0, 3, 4, 0],
        };

        let reader = DenseNodeReader::new(&dense_nodes).expect("dense node reader should be created on valid data");
        let mut result: Vec<DenseNode> = reader.filter_map(|r| r.ok()).collect();

        assert_eq!(result.len(), 2);
        let first = &mut result[0];
        assert_eq!(first.id, 2);
        assert_eq!(first.lat, -3);
        assert_eq!(first.lon, 3);
        assert_eq!(first.key_value_indices, [1, 2]);
        let first_info = first.info.as_ref().unwrap();
        assert_eq!(first_info.uid, Some(5));
        assert_eq!(first_info.timestamp, Some(2));
        assert_eq!(first_info.version, Some(2));
        assert_eq!(first_info.changeset, Some(2));
        assert_eq!(first_info.visible, Some(true));
        assert_eq!(first_info.user_sid, Some(i32::MAX as u32));

        let second = &mut result[1];
        assert_eq!(second.id, 1);
        assert_eq!(second.lat, -2);
        assert_eq!(second.lon, 2);
        assert_eq!(second.key_value_indices, [3, 4]);
        let second_info = second.info.as_ref().unwrap();
        assert_eq!(second_info.uid, Some(4));
        assert_eq!(second_info.timestamp, Some(3));
        assert_eq!(second_info.version, Some(4));
        assert_eq!(second_info.changeset, Some(1));
        assert_eq!(second_info.visible, Some(false));
        assert_eq!(second_info.user_sid, Some(i32::MAX as u32 + 1));
    }

    #[test]
    fn invalid_required_data_lengths() {
        let dense_nodes = |id_count: usize, lat_count: usize, lon_count: usize| pbf::DenseNodes {
            id: vec![0; id_count],
            denseinfo: None,
            lat: vec![0; lat_count],
            lon: vec![0; lon_count],
            keys_vals: vec![],
        };

        assert!(DenseNodeReader::new(&dense_nodes(0, 0, 0)).is_ok());
        assert!(DenseNodeReader::new(&dense_nodes(1, 0, 0)).is_err());
        assert!(DenseNodeReader::new(&dense_nodes(0, 1, 0)).is_err());
        assert!(DenseNodeReader::new(&dense_nodes(0, 0, 1)).is_err());
    }

    #[test]
    fn invalid_user_sid() {
        let dense_info = pbf::DenseInfo {
            user_sid: vec![0, -1],
            ..Default::default()
        };

        let dense_nodes = pbf::DenseNodes {
            id: vec![0, 0],
            denseinfo: Some(dense_info),
            lat: vec![0, 0],
            lon: vec![0, 0],
            keys_vals: vec![],
        };

        let mut reader = DenseNodeReader::new(&dense_nodes).expect("dense node reader should be created on valid data");

        let next = reader.next();
        assert!(next.is_some());
        let next = reader.next();
        assert!(next.is_some());
        assert!(next.unwrap().is_err());
    }
}

/// Utility for reading delta-encoded values directly, like [`pbf::Way::refs`] and [`pbf::Relation::memids`].
pub struct DeltaValueReader<'a, T> {
    remaining: &'a [T],
    accumulated: T,
}

impl<'a, T> DeltaValueReader<'a, T>
where
    T: std::default::Default,
{
    /// Constructs a new `DeltaValueReader` from a slice of values.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rosm_pbf_reader::{pbf, DeltaValueReader};
    ///
    /// fn process_primitive_block(block: pbf::PrimitiveBlock) {
    ///     for group in &block.primitivegroup {
    ///         for way in &group.ways {
    ///             let refs = DeltaValueReader::new(&way.refs);
    ///             for node_id in refs {
    ///                 println!("{}", node_id);
    ///             }
    ///         }
    ///     }
    /// }
    /// ```
    pub fn new(values: &'a [T]) -> Self {
        DeltaValueReader {
            remaining: values,
            accumulated: T::default(),
        }
    }
}

impl<'a, T> Iterator for DeltaValueReader<'a, T>
where
    T: std::ops::AddAssign + std::clone::Clone,
{
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((first, elements)) = self.remaining.split_first() {
            self.accumulated += first.clone();
            self.remaining = elements;
            Some(self.accumulated.clone())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod delta_value_reader_tests {
    use super::*;

    #[test]
    fn empty_input() {
        let mut reader = DeltaValueReader::new(&[] as &[i64]);
        assert_eq!(reader.next(), None);
    }

    #[test]
    fn valid_input() {
        let values = [10, -1, 4, -2];
        let mut reader = DeltaValueReader::new(&values);
        assert_eq!(reader.next(), Some(10));
        assert_eq!(reader.next(), Some(9));
        assert_eq!(reader.next(), Some(13));
        assert_eq!(reader.next(), Some(11));
    }
}
