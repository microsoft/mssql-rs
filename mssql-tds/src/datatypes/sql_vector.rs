// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::core::TdsResult;
use crate::datatypes::sqldatatypes::{
    VECTOR_HEADER_SIZE, VECTOR_MAX_SIZE, VectorBaseType, VectorLayoutFormat, VectorLayoutVersion,
};
use crate::error::Error;

/// Enum representing the typed data stored in a SqlVector.
/// In future, this enum will be extended to support additional base types
/// such as Float16, Int32, etc.
#[derive(Debug, PartialEq, Clone)]
pub enum VectorData {
    /// Single-precision float vector.
    Float32(Vec<f32>),
    /// Half-precision float vector (stored in memory as f32).
    Float16(Vec<f32>),
}

/// Represents a SQL Server Vector data type value.
///
/// The Vector type stores an ordered sequence of elements.
/// Version 1 supports single-precision float (float32) with a maximum of 1998 dimensions
/// and a total size limit of 8000 bytes.
#[derive(Debug, PartialEq, Clone)]
pub struct SqlVector {
    /// Base element type (e.g., float32).
    pub base_type: VectorBaseType, // Preserves original type from SQL Server
    /// Typed dimension data.
    pub data: VectorData,
}

impl SqlVector {
    /// Creates a new SqlVector with the specified values.
    ///
    /// # Arguments
    /// * `values` - The vector dimension values (float32 array)
    ///
    /// # Returns
    /// * `Ok(SqlVector)` if valid
    /// * `Err` if validation fails (too many dimensions, exceeds size limit)
    pub fn try_from_f32(values: Vec<f32>) -> TdsResult<Self> {
        let vector = Self {
            base_type: VectorBaseType::Float32,
            data: VectorData::Float32(values),
        };
        vector.validate_dimensions()?;
        Ok(vector)
    }

    /// Creates a new SqlVector with the specified values, tagged for Float16 storage.
    ///
    /// # Arguments
    /// * `values` - The vector dimension values (passed as f32, but treated as Float16)
    pub fn try_from_f16(values: Vec<f32>) -> TdsResult<Self> {
        let vector = Self {
            base_type: VectorBaseType::Float16,
            data: VectorData::Float16(values),
        };
        vector.validate_dimensions()?;
        Ok(vector)
    }

    /// Creates a SqlVector from raw header fields and raw bytes (used during deserialization).
    /// Validates the TDS header fields then parses and stores the typed data.
    pub(crate) fn try_from_raw(
        layout_format: u8,
        layout_version: u8,
        base_type: u8,
        raw_bytes: Vec<u8>,
    ) -> TdsResult<Self> {
        // Validate TDS header during deserialization
        VectorLayoutFormat::try_from(layout_format)?;
        VectorLayoutVersion::try_from(layout_version)?;
        let base_type_enum = VectorBaseType::try_from(base_type)?;

        // Validate that the payload size matches the element size precisely
        if !raw_bytes
            .len()
            .is_multiple_of(base_type_enum.element_size_bytes())
        {
            return Err(Error::ProtocolError(format!(
                "Malformed vector payload: byte length ({}) is not a multiple of the element size ({})",
                raw_bytes.len(),
                base_type_enum.element_size_bytes()
            )));
        }

        // Parse raw bytes into typed data based on base type
        let data = match base_type_enum {
            VectorBaseType::Float32 => {
                let f32_values: Vec<f32> = raw_bytes
                    .chunks_exact(4)
                    .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                    .collect();
                VectorData::Float32(f32_values)
            }
            VectorBaseType::Float16 => {
                // For Vector V2, SQL Server sends 16-bit Float16 types.
                // Since JavaScript and Python standard APIs do not commonly support native Float16 memory views,
                // we decode these half-precision bits (via the `half` crate) into standard f32 memory representations.
                let f32_values: Vec<f32> = raw_bytes
                    .chunks_exact(2)
                    .map(|chunk| half::f16::from_le_bytes([chunk[0], chunk[1]]).to_f32())
                    .collect();
                VectorData::Float16(f32_values)
            }
        };

        let vector = Self {
            base_type: base_type_enum,
            data,
        };
        vector.validate_dimensions()?;
        Ok(vector)
    }

    /// Returns a reference to the dimension values as a float slice.
    /// Since both Float32 and Float16 are stored natively as f32, this returns `Some` for both.
    /// Returns `None` if the vector data cannot be represented as an f32 slice.
    pub fn as_f32(&self) -> Option<&[f32]> {
        match &self.data {
            VectorData::Float32(v) => Some(v),
            VectorData::Float16(v) => Some(v),
        }
    }

    /// Returns the number of dimensions in this vector.
    pub fn dimension_count(&self) -> u16 {
        match &self.data {
            VectorData::Float32(v) => v.len() as u16,
            VectorData::Float16(v) => v.len() as u16,
        }
    }

    /// Returns the base type of the vector elements as stored in SQL Server.
    /// Note: This may differ from the runtime storage type if conversion was applied.
    /// For example, Float16 from SQL Server might be stored as Float32 for convenience.
    pub fn base_type(&self) -> VectorBaseType {
        self.base_type
    }

    /// Returns the total size in bytes (header + dimension values) on the wire.
    /// Used during serialization (Phase 3).
    pub(crate) fn total_size(&self) -> usize {
        let element_bytes = match &self.data {
            VectorData::Float32(v) => v.len() * self.base_type.element_size_bytes(),
            VectorData::Float16(v) => v.len() * self.base_type.element_size_bytes(),
        };
        VECTOR_HEADER_SIZE + element_bytes
    }

    /// Validates the vector dimensions (count and total size).
    fn validate_dimensions(&self) -> TdsResult<()> {
        let dimension_count = match &self.data {
            VectorData::Float32(v) => v.len(),
            VectorData::Float16(v) => v.len(),
        };

        if dimension_count == 0 {
            return Err(Error::ProtocolError(
                "Vector must have at least one dimension".to_string(),
            ));
        }

        let max_dimensions = self.base_type.max_dimensions();
        if dimension_count > max_dimensions as usize {
            return Err(Error::ProtocolError(format!(
                "Vector dimension count {} exceeds maximum of {} for base type {:?}",
                dimension_count, max_dimensions, self.base_type
            )));
        }

        let total_size = self.total_size();
        if total_size > VECTOR_MAX_SIZE {
            return Err(Error::ProtocolError(format!(
                "Vector total size {} bytes exceeds maximum of {} bytes",
                total_size, VECTOR_MAX_SIZE
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_f32_valid() {
        let values = vec![1.0, 2.0, 3.0];
        let vector = SqlVector::try_from_f32(values);
        assert!(vector.is_ok());
        let vector = vector.unwrap();
        assert_eq!(vector.as_f32(), Some(&[1.0, 2.0, 3.0][..]));
        assert_eq!(vector.dimension_count(), 3);
    }

    #[test]
    fn test_validate_too_many_dimensions() {
        let values = vec![0.0f32; (VectorBaseType::Float32.max_dimensions() + 1) as usize];
        let result = SqlVector::try_from_f32(values);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds maximum"));
    }

    #[test]
    fn test_validate_too_many_dimensions_f16() {
        let values = vec![0.0f32; (VectorBaseType::Float16.max_dimensions() + 1) as usize];
        let result = SqlVector::try_from_f16(values);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds maximum"));
    }

    #[test]
    fn test_validate_unsupported_format() {
        let raw_bytes = 1.0_f32.to_le_bytes().to_vec();
        let result = SqlVector::try_from_raw(
            0x00,
            VectorLayoutVersion::V1 as u8,
            VectorBaseType::Float32 as u8,
            raw_bytes,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid Vector layout format")
        );
    }

    #[test]
    fn test_validate_unsupported_version() {
        let raw_bytes = 1.0_f32.to_le_bytes().to_vec();
        let result = SqlVector::try_from_raw(
            VectorLayoutFormat::V1 as u8,
            0x02,
            VectorBaseType::Float32 as u8,
            raw_bytes,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unsupported Vector layout version")
        );
    }

    #[test]
    fn test_from_raw_float16() {
        let raw_bytes = vec![0, 60]; // 1.0f16 in little-endian (0x3C00)
        let vector = SqlVector::try_from_raw(
            VectorLayoutFormat::V1 as u8,
            VectorLayoutVersion::V1 as u8,
            VectorBaseType::Float16 as u8,
            raw_bytes,
        )
        .unwrap();
        assert_eq!(vector.base_type(), VectorBaseType::Float16);
        assert_eq!(vector.dimension_count(), 1);
        assert_eq!(vector.as_f32().unwrap(), &[1.0]);
    }

    #[test]
    fn test_validate_unsupported_base_type() {
        let raw_bytes = 1.0_f32.to_le_bytes().to_vec();
        let result = SqlVector::try_from_raw(
            VectorLayoutFormat::V1 as u8,
            VectorLayoutVersion::V1 as u8,
            0xFF,
            raw_bytes,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("base type"));
    }

    #[test]
    fn test_total_size() {
        let values = vec![1.0, 2.0, 3.0];
        let vector = SqlVector::try_from_f32(values).unwrap();
        assert_eq!(vector.total_size(), VECTOR_HEADER_SIZE + 3 * 4); // 8 + 12 = 20
    }

    #[test]
    fn test_total_size_float16_uses_wire_size() {
        let raw_bytes = vec![0, 60, 0, 60, 0, 60]; // 3 float16 elements
        let vector = SqlVector::try_from_raw(
            VectorLayoutFormat::V1 as u8,
            VectorLayoutVersion::V1 as u8,
            VectorBaseType::Float16 as u8,
            raw_bytes,
        )
        .unwrap();
        // Native V1 wrapper (f32) has 3 elements. Float16 size is 2 bytes per element.
        // Should calculate 8 (header) + 3*2 = 14 bytes
        assert_eq!(vector.total_size(), VECTOR_HEADER_SIZE + 3 * 2);
    }

    #[test]
    fn test_base_type() {
        let values = vec![1.0, 2.0, 3.0];
        let vector = SqlVector::try_from_f32(values).unwrap();
        assert_eq!(vector.base_type(), VectorBaseType::Float32);
    }
}
