//
//   Copyright 2026 Jeff Bush
//
//   Licensed under the Apache License, Version 2.0 (the "License");
//   you may not use this file except in compliance with the License.
//   You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
//   Unless required by applicable law or agreed to in writing, software
//   distributed under the License is distributed on an "AS IS" BASIS,
//   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//   See the License for the specific language governing permissions and
//   limitations under the License.
//

pub fn get_u16(slice: &[u8], offs: usize) -> u16 {
    u16::from_le_bytes(slice[offs..offs + 2].try_into().unwrap())
}

pub fn get_u32(slice: &[u8], offs: usize) -> u32 {
    u32::from_le_bytes(slice[offs..offs + 4].try_into().unwrap())
}

pub fn get_u64(slice: &[u8], offs: usize) -> u64 {
    u64::from_le_bytes(slice[offs..offs + 8].try_into().unwrap())
}

pub fn set_u16(slice: &mut [u8], offs: usize, val: u16) {
    slice[offs..offs + 2].copy_from_slice(&val.to_le_bytes());
}

pub fn set_u32(slice: &mut [u8], offs: usize, val: u32) {
    slice[offs..offs + 4].copy_from_slice(&val.to_le_bytes());
}

pub fn set_u64(slice: &mut [u8], offs: usize, val: u64) {
    slice[offs..offs + 8].copy_from_slice(&val.to_le_bytes());
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_get_value() {
        let value_array: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(super::get_u16(value_array, 1), 0x0302);
        assert_eq!(super::get_u32(value_array, 2), 0x06050403);
        assert_eq!(super::get_u64(value_array, 1), 0x0908070605040302);
    }

    #[test]
    fn test_set_value() {
        let value_array: &mut [u8] = &mut [0; 16];
        super::set_u16(value_array, 1, 0x1234);
        assert_eq!(value_array[1], 0x34);
        assert_eq!(value_array[2], 0x12);

        super::set_u32(value_array, 1, 0x12345678);
        assert_eq!(value_array[1], 0x78);
        assert_eq!(value_array[2], 0x56);
        assert_eq!(value_array[3], 0x34);
        assert_eq!(value_array[4], 0x12);

        super::set_u64(value_array, 1, 0x12345678abcdef52);
        assert_eq!(value_array[1], 0x52);
        assert_eq!(value_array[2], 0xef);
        assert_eq!(value_array[3], 0xcd);
        assert_eq!(value_array[4], 0xab);
        assert_eq!(value_array[5], 0x78);
        assert_eq!(value_array[6], 0x56);
        assert_eq!(value_array[7], 0x34);
        assert_eq!(value_array[8], 0x12);
    }
}
