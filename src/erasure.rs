// Support erasure coding

use packet::{BlobRecycler, SharedBlob, BLOB_HEADER_SIZE};
use std::result;

//TODO(sakridge) pick these values
pub const NUM_CODED: usize = 20;
pub const MAX_MISSING: usize = 4;
const NUM_DATA: usize = NUM_CODED - MAX_MISSING;

#[derive(Debug, PartialEq, Eq)]
pub enum ErasureError {
    NotEnoughBlocksToDecode,
    DecodeError,
    EncodeError,
    InvalidBlockSize,
}

pub type Result<T> = result::Result<T, ErasureError>;

// k = number of data devices
// m = number of coding devices
// w = word size

extern "C" {
    fn jerasure_matrix_encode(
        k: i32,
        m: i32,
        w: i32,
        matrix: *const i32,
        data_ptrs: *const *const u8,
        coding_ptrs: *const *mut u8,
        size: i32,
    );
    fn jerasure_matrix_decode(
        k: i32,
        m: i32,
        w: i32,
        matrix: *const i32,
        row_k_ones: i32,
        erasures: *const i32,
        data_ptrs: *const *mut u8,
        coding_ptrs: *const *const u8,
        size: i32,
    ) -> i32;
    fn galois_single_divide(a: i32, b: i32, w: i32) -> i32;
}

fn get_matrix(m: i32, k: i32, w: i32) -> Vec<i32> {
    let mut matrix = vec![0; (m * k) as usize];
    for i in 0..m {
        for j in 0..k {
            unsafe {
                matrix[(i * k + j) as usize] = galois_single_divide(1, i ^ (m + j), w);
            }
        }
    }
    matrix
}

pub const ERASURE_W: i32 = 32;

// Generate coding blocks into coding
//   There are some alignment restrictions, blocks should be aligned by 16 bytes
//   which means their size should be >= 16 bytes
pub fn generate_coding_blocks(coding: &mut [&mut [u8]], data: &[&[u8]]) -> Result<()> {
    if data.len() == 0 {
        return Ok(());
    }
    let m = coding.len() as i32;
    let block_len = data[0].len();
    let matrix: Vec<i32> = get_matrix(m, data.len() as i32, ERASURE_W);
    let mut coding_arg = Vec::new();
    let mut data_arg = Vec::new();
    for block in data {
        if block_len != block.len() {
            trace!(
                "data block size incorrect {} expected {}",
                block.len(),
                block_len
            );
            return Err(ErasureError::InvalidBlockSize);
        }
        data_arg.push(block.as_ptr());
    }
    for mut block in coding {
        if block_len != block.len() {
            trace!(
                "coding block size incorrect {} expected {}",
                block.len(),
                block_len
            );
            return Err(ErasureError::InvalidBlockSize);
        }
        coding_arg.push(block.as_mut_ptr());
    }

    unsafe {
        jerasure_matrix_encode(
            data.len() as i32,
            m,
            ERASURE_W,
            matrix.as_ptr(),
            data_arg.as_ptr(),
            coding_arg.as_ptr(),
            data[0].len() as i32,
        );
    }
    Ok(())
}

// Recover data + coding blocks into data blocks
//   data: array of blocks to recover into
//   coding: arry of coding blocks
//   erasures: list of indices in data where blocks should be recovered
pub fn decode_blocks(data: &mut [&mut [u8]], coding: &[&[u8]], erasures: &[i32]) -> Result<()> {
    if data.len() == 0 {
        return Ok(());
    }
    let block_len = data[0].len();
    let matrix: Vec<i32> = get_matrix(coding.len() as i32, data.len() as i32, ERASURE_W);

    // generate coding pointers, blocks should be the same size
    let mut coding_arg: Vec<*const u8> = Vec::new();
    for x in coding.iter() {
        if x.len() != block_len {
            return Err(ErasureError::InvalidBlockSize);
        }
        coding_arg.push(x.as_ptr());
    }

    // generate data pointers, blocks should be the same size
    let mut data_arg: Vec<*mut u8> = Vec::new();
    for x in data.iter_mut() {
        if x.len() != block_len {
            return Err(ErasureError::InvalidBlockSize);
        }
        data_arg.push(x.as_mut_ptr());
    }
    unsafe {
        let ret = jerasure_matrix_decode(
            data.len() as i32,
            coding.len() as i32,
            ERASURE_W,
            matrix.as_ptr(),
            0,
            erasures.as_ptr(),
            data_arg.as_ptr(),
            coding_arg.as_ptr(),
            data[0].len() as i32,
        );
        trace!("jerasure_matrix_decode ret: {}", ret);
        for x in data[erasures[0] as usize][0..8].iter() {
            trace!("{} ", x)
        }
        trace!("");
        if ret < 0 {
            return Err(ErasureError::DecodeError);
        }
    }
    Ok(())
}

// Allocate some coding blobs and insert into the blobs array
pub fn add_coding_blobs(recycler: &BlobRecycler, blobs: &mut Vec<SharedBlob>, consumed: u64) {
    let mut added = 0;
    let blobs_len = blobs.len() as u64;
    for i in consumed..consumed + blobs_len {
        let is = i as usize;
        if is != 0 && ((is + MAX_MISSING) % NUM_CODED) == 0 {
            for _ in 0..MAX_MISSING {
                trace!("putting coding at {}", (i - consumed));
                let new_blob = recycler.allocate();
                let new_blob_clone = new_blob.clone();
                let mut new_blob_l = new_blob_clone.write().unwrap();
                new_blob_l.set_size(0);
                new_blob_l.set_coding().unwrap();
                drop(new_blob_l);
                blobs.insert((i - consumed) as usize, new_blob);
                added += 1;
            }
        }
    }
    info!(
        "add_coding consumed: {} blobs.len(): {} added: {}",
        consumed,
        blobs.len(),
        added
    );
}

// Generate coding blocks in window starting from consumed
pub fn generate_coding(
    window: &mut Vec<Option<SharedBlob>>,
    consumed: usize,
    num_blobs: usize,
) -> Result<()> {
    let mut block_start = consumed - (consumed % NUM_CODED);

    for i in consumed..consumed + num_blobs {
        if (i % NUM_CODED) == (NUM_CODED - 1) {
            let mut data_blobs = Vec::new();
            let mut coding_blobs = Vec::new();
            let mut data_locks = Vec::new();
            let mut data_ptrs: Vec<&[u8]> = Vec::new();
            let mut coding_locks = Vec::new();
            let mut coding_ptrs: Vec<&mut [u8]> = Vec::new();

            info!(
                "generate_coding start: {} end: {} consumed: {} num_blobs: {}",
                block_start,
                block_start + NUM_DATA,
                consumed,
                num_blobs
            );
            for i in block_start..block_start + NUM_DATA {
                let n = i % window.len();
                trace!("window[{}] = {:?}", n, window[n]);
                if window[n].is_none() {
                    trace!("data block is null @ {}", n);
                    return Ok(());
                }
                data_blobs.push(
                    window[n]
                        .clone()
                        .expect("'data_blobs' arr in pub fn generate_coding"),
                );
            }
            let mut max_data_size = 0;
            for b in &data_blobs {
                let lck = b.write().expect("'b' write lock in pub fn generate_coding");
                if lck.meta.size > max_data_size {
                    max_data_size = lck.meta.size;
                }
                data_locks.push(lck);
            }
            trace!("max_data_size: {}", max_data_size);
            for (i, l) in data_locks.iter_mut().enumerate() {
                trace!("i: {} data: {}", i, l.data[0]);
                data_ptrs.push(&l.data[..max_data_size]);
            }

            // generate coding ptr array
            let coding_start = block_start + NUM_DATA;
            let coding_end = block_start + NUM_CODED;
            for i in coding_start..coding_end {
                let n = i % window.len();
                if window[n].is_none() {
                    trace!("coding block is null @ {}", n);
                    return Ok(());
                }
                let w_l = window[n].clone().unwrap();
                w_l.write().unwrap().set_size(max_data_size);
                if w_l.write().unwrap().set_coding().is_err() {
                    return Err(ErasureError::EncodeError);
                }
                coding_blobs.push(
                    window[n]
                        .clone()
                        .expect("'coding_blobs' arr in pub fn generate_coding"),
                );
            }
            for b in &coding_blobs {
                coding_locks.push(
                    b.write()
                        .expect("'coding_locks' arr in pub fn generate_coding"),
                );
            }
            for (i, l) in coding_locks.iter_mut().enumerate() {
                trace!("i: {} coding: {} size: {}", i, l.data[0], max_data_size);
                coding_ptrs.push(&mut l.data_mut()[..max_data_size]);
            }

            generate_coding_blocks(coding_ptrs.as_mut_slice(), &data_ptrs)?;
            debug!(
                "consumed: {} data: {}:{} coding: {}:{}",
                consumed,
                block_start,
                block_start + NUM_DATA,
                coding_start,
                coding_end
            );
            block_start += NUM_CODED;
        }
    }
    Ok(())
}

// Recover missing blocks into window
//   missing blocks should be None, will use re
//   to allocate new ones. Returns err if not enough
//   coding blocks are present to restore
pub fn recover(
    re: &BlobRecycler,
    window: &mut Vec<Option<SharedBlob>>,
    consumed: usize,
    received: usize,
) -> Result<()> {
    //recover with erasure coding
    if received <= consumed {
        return Ok(());
    }
    let num_blocks = (received - consumed) / NUM_CODED;
    let mut block_start = consumed - (consumed % NUM_CODED);

    if num_blocks > 0 {
        debug!(
            "num_blocks: {} received: {} consumed: {}",
            num_blocks, received, consumed
        );
    }

    for i in 0..num_blocks {
        if i > 100 {
            break;
        }
        let mut data_missing = 0;
        let mut coded_missing = 0;
        let coding_start = block_start + NUM_DATA;
        let coding_end = block_start + NUM_CODED;
        trace!(
            "recover: block_start: {} coding_start: {} coding_end: {}",
            block_start,
            coding_start,
            coding_end
        );
        for i in block_start..coding_end {
            let n = i % window.len();
            if window[n].is_none() {
                if i >= coding_start {
                    coded_missing += 1;
                } else {
                    data_missing += 1;
                }
            }
        }
        if (data_missing + coded_missing) != NUM_CODED && (data_missing + coded_missing) != 0 {
            debug!(
                "1: start: {} recovering: data: {} coding: {}",
                block_start, data_missing, coded_missing
            );
        }
        if data_missing > 0 {
            if (data_missing + coded_missing) <= MAX_MISSING {
                debug!(
                    "2: recovering: data: {} coding: {}",
                    data_missing, coded_missing
                );
                let mut blobs: Vec<SharedBlob> = Vec::new();
                let mut locks = Vec::new();
                let mut erasures: Vec<i32> = Vec::new();
                let mut meta = None;
                let mut size = None;
                for i in block_start..coding_end {
                    let j = i % window.len();
                    let mut b = &mut window[j];
                    if b.is_some() {
                        if i >= NUM_DATA && size.is_none() {
                            let bl = b.clone().unwrap();
                            size = Some(bl.read().unwrap().meta.size - BLOB_HEADER_SIZE);
                        }
                        if meta.is_none() {
                            let bl = b.clone().unwrap();
                            meta = Some(bl.read().unwrap().meta.clone());
                        }
                        blobs.push(b.clone().expect("'blobs' arr in pb fn recover"));
                        continue;
                    }
                    let n = re.allocate();
                    *b = Some(n.clone());
                    //mark the missing memory
                    blobs.push(n);
                    erasures.push((i - block_start) as i32);
                }
                erasures.push(-1);
                trace!(
                    "erasures: {:?} data_size: {} header_size: {}",
                    erasures,
                    size.unwrap(),
                    BLOB_HEADER_SIZE
                );
                //lock everything
                for b in &blobs {
                    locks.push(b.write().expect("'locks' arr in pb fn recover"));
                }
                {
                    let mut coding_ptrs: Vec<&[u8]> = Vec::new();
                    let mut data_ptrs: Vec<&mut [u8]> = Vec::new();
                    for (i, l) in locks.iter_mut().enumerate() {
                        if i >= NUM_DATA {
                            trace!("pushing coding: {}", i);
                            coding_ptrs.push(&l.data()[..size.unwrap()]);
                        } else {
                            trace!("pushing data: {}", i);
                            data_ptrs.push(&mut l.data[..size.unwrap()]);
                        }
                    }
                    trace!(
                        "coding_ptrs.len: {} data_ptrs.len {}",
                        coding_ptrs.len(),
                        data_ptrs.len()
                    );
                    decode_blocks(data_ptrs.as_mut_slice(), &coding_ptrs, &erasures)?;
                }
                for i in &erasures[..erasures.len() - 1] {
                    let idx = *i as usize;
                    let data_size = locks[idx].get_data_size().unwrap() - BLOB_HEADER_SIZE as u64;
                    locks[idx].meta = meta.clone().unwrap();
                    locks[idx].set_size(data_size as usize);
                    trace!(
                        "erasures[{}] size: {} data[0]: {}",
                        *i,
                        data_size,
                        locks[idx].data()[0]
                    );
                }
            }
        }
        block_start += NUM_CODED;
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use crdt;
    use erasure;
    use logger;
    use packet::{BlobRecycler, SharedBlob, BLOB_HEADER_SIZE};
    use signature::KeyPair;
    use signature::KeyPairUtil;
    use std::sync::{Arc, RwLock};

    #[test]
    pub fn test_coding() {
        let zero_vec = vec![0; 16];
        let mut vs: Vec<Vec<u8>> = (0..4).map(|i| (i..(16 + i)).collect()).collect();
        let v_orig: Vec<u8> = vs[0].clone();

        let m = 2;
        let mut coding_blocks: Vec<_> = (0..m).map(|_| vec![0u8; 16]).collect();

        {
            let mut coding_blocks_slices: Vec<_> =
                coding_blocks.iter_mut().map(|x| x.as_mut_slice()).collect();
            let v_slices: Vec<_> = vs.iter().map(|x| x.as_slice()).collect();

            assert!(
                erasure::generate_coding_blocks(
                    coding_blocks_slices.as_mut_slice(),
                    v_slices.as_slice()
                ).is_ok()
            );
        }
        trace!("coding blocks:");
        for b in &coding_blocks {
            trace!("{:?}", b);
        }
        let erasure: i32 = 1;
        let erasures = vec![erasure, -1];
        // clear an entry
        vs[erasure as usize].copy_from_slice(zero_vec.as_slice());

        {
            let coding_blocks_slices: Vec<_> = coding_blocks.iter().map(|x| x.as_slice()).collect();
            let mut v_slices: Vec<_> = vs.iter_mut().map(|x| x.as_mut_slice()).collect();

            assert!(
                erasure::decode_blocks(
                    v_slices.as_mut_slice(),
                    coding_blocks_slices.as_slice(),
                    erasures.as_slice(),
                ).is_ok()
            );
        }

        trace!("vs:");
        for v in &vs {
            trace!("{:?}", v);
        }
        assert_eq!(v_orig, vs[0]);
    }

    fn print_window(window: &Vec<Option<SharedBlob>>) {
        for (i, w) in window.iter().enumerate() {
            print!("window({}): ", i);
            if w.is_some() {
                let window_l1 = w.clone().unwrap();
                let window_l2 = window_l1.read().unwrap();
                print!(
                    "index: {:?} meta.size: {} data: ",
                    window_l2.get_index(),
                    window_l2.meta.size
                );
                for i in 0..8 {
                    print!("{} ", window_l2.data()[i]);
                }
            } else {
                print!("null");
            }
            println!("");
        }
    }

    fn generate_window(
        data_len: usize,
        blob_recycler: &BlobRecycler,
        offset: usize,
        num_blobs: usize,
    ) -> (Vec<Option<SharedBlob>>, usize) {
        let mut window = vec![None; 32];
        let mut blobs = Vec::new();
        for i in 0..num_blobs {
            let b = blob_recycler.allocate();
            let b_ = b.clone();
            let mut w = b.write().unwrap();
            w.set_size(data_len);
            for k in 0..data_len {
                w.data_mut()[k] = (k + i) as u8;
            }
            blobs.push(b_);
        }
        erasure::add_coding_blobs(blob_recycler, &mut blobs, offset as u64);
        let blobs_len = blobs.len();

        let d = crdt::NodeInfo::new(
            KeyPair::new().pubkey(),
            "127.0.0.1:1234".parse().unwrap(),
            "127.0.0.1:1235".parse().unwrap(),
            "127.0.0.1:1236".parse().unwrap(),
            "127.0.0.1:1237".parse().unwrap(),
            "127.0.0.1:1238".parse().unwrap(),
        );
        assert!(crdt::Crdt::index_blobs(&d, &blobs, &mut (offset as u64)).is_ok());
        for b in blobs {
            let idx = b.read().unwrap().get_index().unwrap() as usize;
            window[idx] = Some(b);
        }
        (window, blobs_len)
    }

    #[test]
    pub fn test_window_recover_basic() {
        logger::setup();
        let data_len = 16;
        let blob_recycler = BlobRecycler::default();

        // Generate a window
        let offset = 1;
        let num_blobs = erasure::NUM_DATA + 2;
        let (mut window, blobs_len) = generate_window(data_len, &blob_recycler, 0, num_blobs);
        println!("** after-gen-window:");
        print_window(&window);

        // Generate the coding blocks
        assert!(erasure::generate_coding(&mut window, offset, blobs_len).is_ok());
        println!("** after-gen-coding:");
        print_window(&window);

        let erase_offset = offset;
        // Create a hole in the window
        let refwindow = window[erase_offset].clone();
        window[erase_offset] = None;

        // Recover it from coding
        assert!(erasure::recover(&blob_recycler, &mut window, offset, offset + blobs_len).is_ok());
        println!("** after-recover:");
        print_window(&window);

        // Check the result
        let window_l = window[erase_offset].clone().unwrap();
        let window_l2 = window_l.read().unwrap();
        let ref_l = refwindow.clone().unwrap();
        let ref_l2 = ref_l.read().unwrap();
        assert_eq!(
            window_l2.data[..(data_len + BLOB_HEADER_SIZE)],
            ref_l2.data[..(data_len + BLOB_HEADER_SIZE)]
        );
        assert_eq!(window_l2.meta.size, ref_l2.meta.size);
        assert_eq!(window_l2.meta.addr, ref_l2.meta.addr);
        assert_eq!(window_l2.meta.port, ref_l2.meta.port);
        assert_eq!(window_l2.meta.v6, ref_l2.meta.v6);
        assert_eq!(window_l2.get_index().unwrap(), erase_offset as u64);
    }

    //TODO This needs to be reworked
    #[test]
    #[ignore]
    pub fn test_window_recover() {
        logger::setup();
        let blob_recycler = BlobRecycler::default();
        let offset = 4;
        let data_len = 16;
        let num_blobs = erasure::NUM_DATA + 2;
        let (mut window, blobs_len) = generate_window(data_len, &blob_recycler, offset, num_blobs);
        println!("** after-gen:");
        print_window(&window);
        assert!(erasure::generate_coding(&mut window, offset, blobs_len).is_ok());
        println!("** after-coding:");
        print_window(&window);
        let refwindow = window[offset + 1].clone();
        window[offset + 1] = None;
        window[offset + 2] = None;
        window[offset + erasure::NUM_CODED + 3] = None;
        window[offset + (2 * erasure::NUM_CODED) + 0] = None;
        window[offset + (2 * erasure::NUM_CODED) + 1] = None;
        window[offset + (2 * erasure::NUM_CODED) + 2] = None;
        let window_l0 = &(window[offset + (3 * erasure::NUM_CODED)]).clone().unwrap();
        window_l0.write().unwrap().data[0] = 55;
        println!("** after-nulling:");
        print_window(&window);
        assert!(erasure::recover(&blob_recycler, &mut window, offset, offset + blobs_len).is_ok());
        println!("** after-restore:");
        print_window(&window);
        let window_l = window[offset + 1].clone().unwrap();
        let ref_l = refwindow.clone().unwrap();
        assert_eq!(
            window_l.read().unwrap().data()[..data_len],
            ref_l.read().unwrap().data()[..data_len]
        );
    }
}
