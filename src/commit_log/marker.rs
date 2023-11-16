use crate::{
    serde::{Deserializable, DeserializeError, Serializable, SerializeError},
    Value,
};
use std::io::{Read, Write};

#[derive(Debug)]
pub enum Marker {
    Start(u32),
    Item(Value),
    End(u32),
}

const MARKER_START_TAG: u8 = 0;
const MARKER_ITEM_TAG: u8 = 1;
const MARKER_END_TAG: u8 = 2;

impl Serializable for Marker {
    fn serialize<W: Write>(&self, writer: &mut W) -> Result<(), SerializeError> {
        use Marker::{End, Item, Start};

        match self {
            Start(val) => {
                writer.write_all(&[MARKER_START_TAG])?;
                writer.write_all(&val.to_be_bytes())?;
            }
            Item(value) => {
                writer.write_all(&[MARKER_ITEM_TAG])?;
                value.serialize(writer)?;
            }
            End(val) => {
                writer.write_all(&[MARKER_END_TAG])?;
                writer.write_all(&val.to_be_bytes())?;
            }
        }
        Ok(())
    }
}

impl Deserializable for Marker {
    fn deserialize<R: Read>(reader: &mut R) -> Result<Self, DeserializeError> {
        use Marker::{End, Item, Start};

        let mut tag = [0u8; 1];
        reader.read_exact(&mut tag)?;

        match tag[0] {
            MARKER_START_TAG => {
                let mut val_bytes = [0u8; 4];
                reader.read_exact(&mut val_bytes)?;
                let val = u32::from_be_bytes(val_bytes);
                Ok(Start(val))
            }
            MARKER_ITEM_TAG => {
                let value = Value::deserialize(reader)?;
                Ok(Item(value))
            }
            MARKER_END_TAG => {
                let mut val_bytes = [0u8; 4];
                reader.read_exact(&mut val_bytes)?;
                let val = u32::from_be_bytes(val_bytes);
                Ok(End(val))
            }
            tag => Err(DeserializeError::InvalidTag(tag)),
        }
    }
}