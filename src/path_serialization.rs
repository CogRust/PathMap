
// GOAT both functions should be tested on long paths (larger than chunk size)
use libz_ng_sys::*;
use crate::PathMap;
use crate::TrieValue;
use crate::alloc::Allocator;
use crate::zipper::{ZipperReadOnlyIteration, ZipperWriting};

#[derive(Debug, Clone, Copy)]
pub struct SerializationStats {
  pub bytes_out  : usize, 
  pub bytes_in   : usize, 
  pub path_count : usize
}
#[derive(Debug, Clone, Copy)]
pub struct DeserializationStats {
  pub bytes_in   : usize, 
  pub bytes_out  : usize, 
  pub path_count : usize
}

pub fn serialize_paths_<'a, V : TrieValue, RZ : ZipperReadOnlyIteration<'a, V>, W: std::io::Write>(rz: RZ, target: &mut W) -> std::io::Result<SerializationStats> {
  serialize_paths(rz, target, |_, _, _| {})
}
/// Serialize all paths in under path `k`
/// Warning: the size of the individual path serialization can be double exponential in the size of the PathMap
/// Returns the target output, total serialized bytes (uncompressed), and total number of paths
pub fn serialize_paths<'a, V : TrieValue, RZ : ZipperReadOnlyIteration<'a, V>, W: std::io::Write, F: FnMut(usize, &[u8], &V) -> ()>(mut rz: RZ, target: &mut W, mut fv: F) -> std::io::Result<SerializationStats> {
  const CHUNK: usize = 4096; // not tuned yet
  let mut buffer = [0u8; CHUNK];
  #[allow(invalid_value)] //Squish the warning about a Null function ptr, because zlib uses a default allocator if the the ptr is NULL
  //I filed https://github.com/rust-lang/libz-sys/issues/243 to track this issue, and I confirmed the easy fix works, but I didn't submit
  // a PR because their build and validation process is very confusing.
  let mut strm: z_stream = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
  let mut ret = unsafe { zng_deflateInit(&mut strm, 7) };
  if ret != Z_OK { panic!("init failed") }

  let mut total_paths : usize = 0;
  while let Some(v) = rz.to_next_get_value() {
    let p = rz.path();
    fv(total_paths, p, v);
    let l = p.len();
    let mut lin = (l as u32).to_le_bytes();
    strm.avail_in = 4;
    strm.next_in = lin.as_mut_ptr();

    // todo (Adam): this is stupid/simple code; the following two blocks should be merged and write out the path length and path together
    loop {
      strm.avail_out = CHUNK as _;
      strm.next_out = buffer.as_mut_ptr();
      ret = unsafe { deflate(&mut strm, Z_NO_FLUSH) };
      assert_ne!(ret, Z_STREAM_ERROR);
      let have = CHUNK - strm.avail_out as usize;
      target.write_all(&mut buffer[..have])?;
      if strm.avail_out != 0 { break }
    }
    assert_eq!(strm.avail_in, 0);

    strm.avail_in = l as _;
    strm.next_in = p.as_ptr().cast_mut();
    loop {
      strm.avail_out = CHUNK as _;
      strm.next_out = buffer.as_mut_ptr();
      ret = unsafe { deflate(&mut strm, Z_NO_FLUSH) };
      assert_ne!(ret, Z_STREAM_ERROR);
      let have = CHUNK - strm.avail_out as usize;
      target.write_all(&mut buffer[..have])?;
      if strm.avail_out != 0 { break }
    }
    assert_eq!(strm.avail_in, 0);


    total_paths += 1;
  }
  loop {
    strm.avail_out = CHUNK as _;
    strm.next_out = buffer.as_mut_ptr();
    ret = unsafe { deflate(&mut strm, Z_FINISH) };
    let have = CHUNK - strm.avail_out as usize;
    target.write_all(&buffer[..have])?;
    if ret == Z_STREAM_END { break; }
    assert_eq!(ret, Z_OK);
  }
  ret = unsafe { deflateEnd(&mut strm) };
  assert_eq!(ret, Z_OK);

  Ok(SerializationStats {
    bytes_out  : strm.total_out, 
    bytes_in   : strm.total_in,
    path_count : total_paths
  })
}

pub fn deserialize_paths_<V: TrieValue, A: Allocator, WZ : ZipperWriting<V, A>, R: std::io::Read>(wz: WZ, source: R, v: V) -> std::io::Result<DeserializationStats> {
  deserialize_paths(wz, source, |_, _| v.clone())
}

/// Deserialize bytes that were serialized by `serialize_paths` under path `k`
/// Returns the source input, total deserialized bytes (uncompressed), and total number of path insert attempts
pub fn deserialize_paths<V: TrieValue, A: Allocator, WZ : ZipperWriting<V, A>, R: std::io::Read, F: Fn(usize, &[u8]) -> V>(mut wz: WZ, mut source: R, fv: F) -> std::io::Result<DeserializationStats> {
  use libz_ng_sys::*;
  const IN: usize = 1024;
  const OUT: usize = 2048;
  let mut ibuffer = [0u8; IN];
  let mut obuffer = [0u8; OUT];
  let mut l = 0u32;
  let mut lbuf = [0u8; 4];
  let mut lbuf_offset = 0;
  let mut finished_path = true;
  let mut total_paths : usize = 0usize;
  #[allow(invalid_value)] //Squish the warning about a Null function ptr, because zlib uses a default allocator if the the ptr is NULL
  let mut strm: z_stream = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
  let mut ret = unsafe { zng_inflateInit(&mut strm) };
  if ret != Z_OK { return Err(std::io::Error::new(std::io::ErrorKind::Other, "failed to init zlib-ng inflate")) }
  let mut submap = PathMap::new_in(wz.alloc());
  let mut wz_buf = vec![];
  // if statement in loop that emulates goto for the many to many ibuffer-obuffer relation
  'reading: loop {
    strm.avail_in = source.read(&mut ibuffer)? as _;
    if strm.avail_in == 0 { break; }
    strm.next_in = &mut ibuffer as _;

    'decompressing: loop {
      strm.avail_out = OUT as _;
      strm.next_out = obuffer.as_mut_ptr();
      let mut pos = 0usize;

      ret = unsafe { inflate(&mut strm, Z_NO_FLUSH) };
      if ret == Z_STREAM_ERROR { return Err(std::io::Error::new(std::io::ErrorKind::Other, "Z_STREAM_ERROR")) }
      if strm.avail_out as usize == OUT {
        if ret == Z_STREAM_END { break 'reading }
        else { continue 'reading }
      }
      let end = OUT - strm.avail_out as usize;

      'descending: loop {
        if finished_path {
          let have = (end - pos).min(4-lbuf_offset);
          lbuf[lbuf_offset..lbuf_offset+have].copy_from_slice(&obuffer[pos..pos+have]);
          pos += have;
          lbuf_offset += have;
          if lbuf_offset == 4 {
            l = u32::from_le_bytes(lbuf);
            lbuf_offset = 0;
          } else {
            if strm.avail_in == 0 { continue 'reading }
            else { continue 'decompressing }
          }
        }

        if pos + l as usize <= end {
          wz_buf.extend(&obuffer[pos..pos + l as usize]);
          let v = fv(total_paths, &wz_buf[..]);
          submap.set_val_at(&wz_buf[..], v);
          wz_buf.clear();
          total_paths += 1;
          pos += l as usize;
          finished_path = true;
          if pos == end { continue 'decompressing }
          else { continue 'descending }
        } else {
          wz_buf.extend(&obuffer[pos..end]);
          finished_path = false;
          l -= (end-pos) as u32;
          if strm.avail_in == 0 { continue 'reading }
          else { continue 'decompressing }
        }
      }
    }
  }
  wz.graft_map(submap);

  unsafe { inflateEnd(&mut strm) };

  
  Ok(DeserializationStats {
    bytes_in   : strm.total_in,
    bytes_out  : strm.total_out, 
    path_count : total_paths
  })
}

#[cfg(test)]
mod test {
  use crate::zipper::{ZipperIteration, ZipperMoving};
  use super::*;

  #[cfg(not(miri))] // miri really hates the zlib-ng-sys C API
  #[test]
  fn path_serialize_deserialize() {
    let mut btm = PathMap::new();
    let rs = ["arrow", "bow", "cannon", "roman", "romane", "romanus", "romulus", "rubens", "ruber", "rubicon", "rubicundus", "rom'i"];
    rs.iter().for_each(|r| { btm.set_val_at(r.as_bytes(), ()); });
    let mut v = vec![];
    match serialize_paths_(btm.read_zipper(), &mut v) {
      Ok(SerializationStats { bytes_out : c, bytes_in : bw, path_count : pw}) => {
        println!("ser {} {} {}", c, bw, pw);
        println!("vlen {}", v.len());

        let mut restored_btm = PathMap::new();
        match deserialize_paths_(restored_btm.write_zipper(), v.as_slice(), ()) {
          Ok(DeserializationStats { bytes_in : c, bytes_out : bw, path_count : pw}) => {
            println!("de {} {} {}", c, bw, pw);

            let mut lrz = restored_btm.read_zipper();
            while lrz.to_next_val() {
              assert!(btm.contains(lrz.path()), "{}", std::str::from_utf8(lrz.path()).unwrap());
            }

            let mut rrz = btm.read_zipper();
            while rrz.to_next_val() {
              assert!(restored_btm.contains(rrz.path()));
            }
          }
          Err(e) => { println!("de e {}", e) }
        }
      }
      Err(e) => { println!("ser e {}", e) }
    }
  }

  #[cfg(not(miri))] // miri really hates the zlib-ng-sys C API
  #[test]
  fn path_serialize_deserialize_blow_out_buffer() {
    for zeros in 0..10 {
      println!("{zeros} zeros");
      let mut btm = PathMap::new();
      let mut rs = vec![];
      for i in 0..400 {
        rs.push(format!("{}{}{}{}", "0".repeat(zeros), i/100, (i/10)%10, i%10))
      }
      rs.iter().for_each(|r| { btm.set_val_at(r.as_bytes(), ()); });

      let mut v = vec![];
      match serialize_paths_(btm.read_zipper(), &mut v) {
        Ok(SerializationStats { bytes_out : c, bytes_in : bw, path_count : pw}) => {
          println!("ser {} {} {}", c, bw, pw);
          println!("vlen {}", v.len());

          let mut restored_btm = PathMap::new();
          match deserialize_paths_(restored_btm.write_zipper(), v.as_slice(), ()) {
          Ok(DeserializationStats { bytes_in : c, bytes_out : bw, path_count : pw}) => {
              println!("de {} {} {}", c, bw, pw);

              let mut lrz = restored_btm.read_zipper();
              while lrz.to_next_val() {
                assert!(btm.contains(lrz.path()), "{}", std::str::from_utf8(lrz.path()).unwrap());
              }

              let mut rrz = btm.read_zipper();
              while rrz.to_next_val() {
                assert!(restored_btm.contains(rrz.path()));
              }
            }
            Err(e) => { println!("de e {}", e) }
          }
        }
        Err(e) => { println!("ser e {}", e) }
      }
    }
  }

  #[cfg(not(miri))] // miri really hates the zlib-ng-sys C API
  #[test]
  fn path_serialize_deserialize_values() {
    let mut btm = PathMap::new();
    let rs = ["arrow", "bow", "cannon", "roman", "romane", "romanus", "romulus", "rubens", "ruber", "rubicon", "rubicundus", "rom'i"];
    rs.iter().enumerate().for_each(|(i, r)| { btm.set_val_at(r.as_bytes(), i); });
    let mut values = vec![];
    let mut v = vec![];
    match serialize_paths(btm.read_zipper(), &mut v,
                          |c, _p, value| { assert_eq!(values.len(), c); values.push(*value) }) {
      Ok(SerializationStats { bytes_out : c, bytes_in : bw, path_count : pw}) => {
        println!("ser {} {} {}", c, bw, pw);
        println!("vlen {}", v.len());

        let mut restored_btm = PathMap::new();
        match deserialize_paths(restored_btm.write_zipper(), v.as_slice(), |c, _p| values[c]) {
          Ok(DeserializationStats { bytes_in : c, bytes_out : bw, path_count : pw}) => {
            println!("de {} {} {}", c, bw, pw);

            let mut lrz = restored_btm.read_zipper();
            while let Some(v) = lrz.to_next_get_value() {
              assert_eq!(btm.get_val_at(lrz.path()), Some(v));
            }

            let mut rrz = btm.read_zipper();
            while let Some(v) = rrz.to_next_get_value() {
              assert_eq!(restored_btm.get_val_at(rrz.path()), Some(v));
            }
          }
          Err(e) => { println!("de e {}", e) }
        }
      }
      Err(e) => { println!("ser e {}", e) }
    }
  }
}
