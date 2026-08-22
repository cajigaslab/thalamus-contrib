use std::io::Error;

use bytebuffer::{ByteBuffer, ByteReader};

#[derive(PartialEq)]
pub enum BlockType {
  DATA,
  CMD
}
pub const ID_ENABLE: u8 = 0;
pub const ID_SET_SAMPLE_RATE: u8 = 1;
pub const ID_SET_CHANNEL_MASK: u8 = 2;
pub const ID_STIMULATE: u8 = 3;
pub const ID_ECHO: u8 = 4;
pub const ID_CUSTOM_CONFIG: u8 = 5;
pub const ID_RESET_DEVICE: u8 = 6;
pub const ID_RHD: u8 = 4;
pub const CONFIG_AMPLIFIER_FAST_SETTLE: u8 = 1;

pub struct Block<'a> {
  pub block_type: BlockType,
  pub block_id: u8,
  pub data: &'a [u8],
  pub first_point_idx: i16,
  pub first_channel_sampled: u8
}

impl<'a> Block<'a> {
  pub fn cmd(block_id: u8, data: &'a [u8]) -> Block {
    Block {
      block_type: BlockType::CMD,
      block_id,
      data,
      first_point_idx: -1,
      first_channel_sampled: 0
    }
  }

  pub fn decode(data: &'a [u8]) -> Result<Block<'a>, Error> {
    let mut reader = ByteReader::from_bytes(data);
    reader.set_endian(bytebuffer::Endian::BigEndian);
    
    let id_byte = reader.read_u8()?;
    let is_command_block = (id_byte & 0b10000000) != 0;
    let block_id = id_byte & 0x7F;
    let new_first_channel = (id_byte >> 3) & 0x0F;
    let new_block_id = id_byte & 0x07;

    if is_command_block {
      let data = &data[reader.get_rpos()..];
      Ok(Block{ block_type: BlockType::CMD, block_id, data , first_point_idx: -1, first_channel_sampled: 0 })
    } else {
      //Block(BlockType.CMD, block_id, data[2..])
      let first_point_idx = reader.read_i16()?;
      let data = &data[reader.get_rpos()..];
      if new_block_id == 4 {
        Ok(Block{ block_type: BlockType::CMD, block_id: new_block_id, data: data, first_point_idx, first_channel_sampled: new_first_channel })
      } else {
        Ok(Block{ block_type: BlockType::CMD, block_id, data: data, first_point_idx, first_channel_sampled: 0 })
      }
    }
  }

  pub fn encode(&self) -> Vec<u8> {
    let prefix_size = if self.block_type == BlockType::DATA {
        4
    } else {
        2
    };
    let total_size = prefix_size + self.data.len();
    let mut buffer = ByteBuffer::new();
    buffer.set_endian(bytebuffer::Endian::BigEndian);
    buffer.write_u8(total_size.try_into().unwrap());

    if self.block_type == BlockType::DATA {
      buffer.write_u8(self.block_id);
      buffer.write_i16(self.first_point_idx);
    } else {
      buffer.write_u8(self.block_id | 0x80);
    }
    buffer.write_bytes(self.data);
    
    buffer.into_vec()
  }
}

pub fn decode_block_packet<'a>(data: &'a [u8]) -> Result<Vec<Block<'a>>, Error> {
  let mut pos = 0;
  let mut result = Vec::<Block>::new();
  while pos < data.len() {
    let len = usize::from(data[pos]);
    result.push(Block::decode(&data[pos..(pos+len)])?);
    pos += len;
  }
  Ok(result)
}
