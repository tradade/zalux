use super::wire::*;

#[test]
fn fake_relay_reply_unwraps() {
    let call_id: u32 = 900;
    let paddr = b"1.2.3.4|9";
    let mut reply = vec![0u8; 35 + paddr.len()];
    reply[0] = 0x02;
    reply[1] = 0x7e;
    reply[18..20].copy_from_slice(&11u16.to_le_bytes());
    reply[20] = 2;
    reply[21..25].copy_from_slice(&0u32.to_le_bytes());
    reply[25..29].copy_from_slice(&call_id.to_le_bytes());
    reply[29..33].copy_from_slice(&0xdeadbeefu32.to_le_bytes());
    // `u16String`: LE16 length (2 byte) rồi mới đến bytes địa chỉ.
    reply[33..35].copy_from_slice(&(paddr.len() as u16).to_le_bytes());
    reply[35..35 + paddr.len()].copy_from_slice(paddr);
    let p = unwrap_packet(&reply).expect("parse");
    assert!(p.is_control);
    assert_eq!(p.cmd, cmd::INIT_CALL_CALLER);
    assert_eq!(p.res, Some(0));
    assert_eq!(p.call_id, Some(900));
    assert_eq!(p.server_token, Some(0xdeadbeef));
    let addr = p.public_addr.map(|a| String::from_utf8_lossy(&a).into_owned());
    assert_eq!(addr.as_deref(), Some("1.2.3.4|9"), "public_addr parse");
}
