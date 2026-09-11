use mcu_update::flash::picoboot::{Uf2Error, decode_uf2};

const UF2_BLOCK_SIZE: usize = 512;

#[test]
fn decodes_a_klipper_style_rp2040_uf2_image() {
    let image = uf2_block(0x1000_0100, 0, 2, &[0xaa; 256])
        .into_iter()
        .chain(uf2_block(0x1000_0200, 1, 2, &[0xbb; 256]))
        .collect::<Vec<_>>();

    let decoded = decode_uf2(&image).expect("valid UF2 image");

    assert_eq!(decoded.address, 0x1000_0100);
    assert_eq!(decoded.bytes.len(), 512);
    assert_eq!(decoded.bytes[0], 0xaa);
    assert_eq!(decoded.bytes[256], 0xbb);
}

#[test]
fn rejects_a_sparse_uf2_image() {
    let image = uf2_block(0x1000_0100, 0, 2, &[0xaa; 256])
        .into_iter()
        .chain(uf2_block(0x1000_0300, 1, 2, &[0xbb; 256]))
        .collect::<Vec<_>>();

    assert!(matches!(decode_uf2(&image), Err(Uf2Error::NonContiguous)));
}

fn uf2_block(address: u32, block_no: u32, num_blocks: u32, payload: &[u8]) -> Vec<u8> {
    let mut block = vec![0; UF2_BLOCK_SIZE];
    block[0..4].copy_from_slice(&0x0a32_4655_u32.to_le_bytes());
    block[4..8].copy_from_slice(&0x9e5d_5157_u32.to_le_bytes());
    block[8..12].copy_from_slice(&0x0000_2000_u32.to_le_bytes());
    block[12..16].copy_from_slice(&address.to_le_bytes());
    block[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    block[20..24].copy_from_slice(&block_no.to_le_bytes());
    block[24..28].copy_from_slice(&num_blocks.to_le_bytes());
    block[28..32].copy_from_slice(&0xe48b_ff56_u32.to_le_bytes());
    block[32..32 + payload.len()].copy_from_slice(payload);
    block[508..512].copy_from_slice(&0x0ab1_6f30_u32.to_le_bytes());
    block
}
