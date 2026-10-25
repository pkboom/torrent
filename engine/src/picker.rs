//! Piece selection: rarest-first, finishing started pieces first, with endgame duplication.

use rand::Rng;
use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;

pub const BLOCK_SIZE: u32 = 16 * 1024;
/// How many peers may be asked for the same block during endgame.
const ENDGAME_DUPLICATES: u8 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PieceState {
    Missing,
    InProgress,
    Verifying,
    Have,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    Free,
    Requested(u8),
    Received,
}

struct Partial {
    buf: Vec<u8>,
    blocks: Vec<Block>,
    received: usize,
    /// Peers that sent blocks of this piece, blamed if the hash check fails.
    contributors: HashSet<SocketAddr>,
}

#[derive(Clone, Copy, Hash, Eq, PartialEq, Debug)]
pub struct BlockReq {
    pub piece: u32,
    pub begin: u32,
    pub len: u32,
}

pub enum BlockResult {
    Ignored,
    Stored,
    PieceDone(Vec<u8>, HashSet<SocketAddr>),
}

pub struct Picker {
    piece_length: u64,
    total_length: u64,
    state: Vec<PieceState>,
    availability: Vec<u32>,
    partial: BTreeMap<u32, Partial>,
    missing: usize,
    have_bytes: u64,
}

impl Picker {
    pub fn new(piece_length: u64, total_length: u64, have: &[bool]) -> Self {
        let mut p = Picker {
            piece_length,
            total_length,
            state: have
                .iter()
                .map(|&h| if h { PieceState::Have } else { PieceState::Missing })
                .collect(),
            availability: vec![0; have.len()],
            partial: BTreeMap::new(),
            missing: have.iter().filter(|h| !**h).count(),
            have_bytes: 0,
        };
        p.have_bytes = (0..have.len() as u32).filter(|&i| have[i as usize]).map(|i| p.piece_size(i)).sum();
        p
    }

    pub fn piece_size(&self, i: u32) -> u64 {
        (self.total_length - i as u64 * self.piece_length).min(self.piece_length)
    }

    pub fn have_bytes(&self) -> u64 {
        self.have_bytes
    }

    pub fn has(&self, i: u32) -> bool {
        self.state[i as usize] == PieceState::Have
    }

    pub fn is_complete(&self) -> bool {
        self.have_bytes == self.total_length
    }

    pub fn bitfield(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.state.len().div_ceil(8)];
        for (i, s) in self.state.iter().enumerate() {
            if *s == PieceState::Have {
                out[i / 8] |= 0x80 >> (i % 8);
            }
        }
        out
    }

    pub fn add_peer(&mut self, bits: &[bool]) {
        for (a, &b) in self.availability.iter_mut().zip(bits) {
            *a += b as u32;
        }
    }

    pub fn remove_peer(&mut self, bits: &[bool]) {
        for (a, &b) in self.availability.iter_mut().zip(bits) {
            *a = a.saturating_sub(b as u32);
        }
    }

    pub fn peer_has(&mut self, i: u32) {
        if let Some(a) = self.availability.get_mut(i as usize) {
            *a += 1;
        }
    }

    /// Whether the peer has any piece we still need.
    pub fn wants_from(&self, bits: &[bool]) -> bool {
        self.state
            .iter()
            .zip(bits)
            .any(|(s, &b)| b && matches!(s, PieceState::Missing | PieceState::InProgress))
    }

    fn block_len(&self, piece: u32, block: usize) -> u32 {
        let size = self.piece_size(piece);
        (size - block as u64 * BLOCK_SIZE as u64).min(BLOCK_SIZE as u64) as u32
    }

    /// Picks up to `max` blocks to request from a peer with `bits`.
    /// `mine(piece, begin)` tells whether that block is already outstanding on this peer.
    pub fn pick(&mut self, bits: &[bool], max: usize, mine: impl Fn(u32, u32) -> bool) -> Vec<BlockReq> {
        let mut out = Vec::new();
        if max == 0 {
            return out;
        }
        let pieces: Vec<u32> = self.partial.keys().copied().filter(|&i| bits[i as usize]).collect();
        for i in pieces {
            self.take_free(i, max, &mut out);
            if out.len() >= max {
                return out;
            }
        }
        while out.len() < max {
            let Some(i) = self.choose_new(bits) else { break };
            let size = self.piece_size(i) as usize;
            let nblocks = size.div_ceil(BLOCK_SIZE as usize);
            self.partial.insert(
                i,
                Partial { buf: vec![0; size], blocks: vec![Block::Free; nblocks], received: 0, contributors: HashSet::new() },
            );
            self.state[i as usize] = PieceState::InProgress;
            self.missing -= 1;
            self.take_free(i, max, &mut out);
        }
        if out.is_empty() && self.missing == 0 {
            // Endgame: everything is requested somewhere; race other peers for the stragglers.
            let mut extra = Vec::new();
            'outer: for (&i, p) in self.partial.iter_mut() {
                if !bits[i as usize] {
                    continue;
                }
                for (bi, b) in p.blocks.iter_mut().enumerate() {
                    if let Block::Requested(n) = b {
                        if *n < ENDGAME_DUPLICATES && !mine(i, bi as u32 * BLOCK_SIZE) {
                            *n += 1;
                            extra.push((i, bi));
                            if out.len() + extra.len() >= max {
                                break 'outer;
                            }
                        }
                    }
                }
            }
            for (i, bi) in extra {
                out.push(BlockReq { piece: i, begin: bi as u32 * BLOCK_SIZE, len: self.block_len(i, bi) });
            }
        }
        out
    }

    fn take_free(&mut self, piece: u32, max: usize, out: &mut Vec<BlockReq>) {
        let size = self.piece_size(piece);
        let p = self.partial.get_mut(&piece).unwrap();
        for (bi, b) in p.blocks.iter_mut().enumerate() {
            if out.len() >= max {
                break;
            }
            if *b == Block::Free {
                *b = Block::Requested(1);
                let begin = bi as u32 * BLOCK_SIZE;
                let len = (size - begin as u64).min(BLOCK_SIZE as u64) as u32;
                out.push(BlockReq { piece, begin, len });
            }
        }
    }

    /// Rarest missing piece the peer has, with a random starting point to break ties.
    fn choose_new(&self, bits: &[bool]) -> Option<u32> {
        let n = self.state.len();
        if n == 0 || self.missing == 0 {
            return None;
        }
        let start = rand::thread_rng().gen_range(0..n);
        let mut best: Option<(u32, usize)> = None;
        for k in 0..n {
            let i = (start + k) % n;
            if self.state[i] == PieceState::Missing && bits[i] {
                let a = self.availability[i];
                if best.is_none_or(|(ba, _)| a < ba) {
                    best = Some((a, i));
                    if a <= 1 {
                        break;
                    }
                }
            }
        }
        best.map(|(_, i)| i as u32)
    }

    pub fn release(&mut self, req: &BlockReq) {
        if let Some(p) = self.partial.get_mut(&req.piece) {
            let b = &mut p.blocks[(req.begin / BLOCK_SIZE) as usize];
            if let Block::Requested(n) = b {
                *b = if *n > 1 { Block::Requested(*n - 1) } else { Block::Free };
            }
        }
    }

    pub fn on_block(&mut self, piece: u32, begin: u32, data: &[u8], from: SocketAddr) -> BlockResult {
        if begin % BLOCK_SIZE != 0 {
            return BlockResult::Ignored;
        }
        let bi = (begin / BLOCK_SIZE) as usize;
        let Some(expected) = self.partial.get(&piece).and_then(|p| p.blocks.get(bi)).map(|_| self.block_len(piece, bi)) else {
            return BlockResult::Ignored;
        };
        if data.len() != expected as usize {
            return BlockResult::Ignored;
        }
        let p = self.partial.get_mut(&piece).unwrap();
        if p.blocks[bi] == Block::Received {
            return BlockResult::Ignored;
        }
        p.blocks[bi] = Block::Received;
        p.buf[begin as usize..begin as usize + data.len()].copy_from_slice(data);
        p.received += 1;
        p.contributors.insert(from);
        if p.received < p.blocks.len() {
            return BlockResult::Stored;
        }
        let p = self.partial.remove(&piece).unwrap();
        self.state[piece as usize] = PieceState::Verifying;
        BlockResult::PieceDone(p.buf, p.contributors)
    }

    /// Called when a session starts: work that belonged to stopped peer tasks is handed back.
    /// Pieces abandoned mid-verification are fetched again; received blocks are kept.
    pub fn reset_transient(&mut self) {
        for s in self.state.iter_mut().filter(|s| **s == PieceState::Verifying) {
            *s = PieceState::Missing;
            self.missing += 1;
        }
        for p in self.partial.values_mut() {
            for b in p.blocks.iter_mut().filter(|b| matches!(b, Block::Requested(_))) {
                *b = Block::Free;
            }
        }
        self.availability.iter_mut().for_each(|a| *a = 0);
    }

    pub fn piece_verified(&mut self, piece: u32, ok: bool) {
        if self.state[piece as usize] != PieceState::Verifying {
            return;
        }
        if ok {
            self.state[piece as usize] = PieceState::Have;
            self.have_bytes += self.piece_size(piece);
        } else {
            self.state[piece as usize] = PieceState::Missing;
            self.missing += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1);

    #[test]
    fn picks_and_completes() {
        let mut p = Picker::new(BLOCK_SIZE as u64 * 2, BLOCK_SIZE as u64 * 3, &[false, false]);
        let all = [true, true];
        p.add_peer(&all);
        let reqs = p.pick(&all, 10, |_, _| false);
        assert_eq!(reqs.len(), 3);
        let mine: HashSet<(u32, u32)> = reqs.iter().map(|r| (r.piece, r.begin)).collect();
        assert!(p.pick(&all, 10, |i, b| mine.contains(&(i, b))).is_empty());
        // A different peer gets endgame duplicates.
        assert_eq!(p.pick(&all, 10, |_, _| false).len(), 3);
        for r in &reqs {
            let data = vec![1u8; r.len as usize];
            if let BlockResult::PieceDone(buf, _) = p.on_block(r.piece, r.begin, &data, PEER) {
                assert_eq!(buf.len() as u64, p.piece_size(r.piece));
                p.piece_verified(r.piece, true);
            }
        }
        assert!(p.is_complete());
    }

    #[test]
    fn failed_hash_is_repicked() {
        let mut p = Picker::new(BLOCK_SIZE as u64, BLOCK_SIZE as u64, &[false]);
        let r = p.pick(&[true], 5, |_, _| false)[0];
        let BlockResult::PieceDone(_, from) = p.on_block(r.piece, r.begin, &vec![0; r.len as usize], PEER) else { panic!() };
        assert!(from.contains(&PEER));
        p.piece_verified(0, false);
        assert_eq!(p.pick(&[true], 5, |_, _| false).len(), 1);
    }
}

#[cfg(test)]
mod reset_tests {
    use super::*;

    #[test]
    fn reset_releases_abandoned_work() {
        let mut p = Picker::new(BLOCK_SIZE as u64, BLOCK_SIZE as u64 * 2, &[false, false]);
        let reqs = p.pick(&[true, true], 1, |_, _| false);
        let peer = "127.0.0.1:1".parse().unwrap();
        // Piece being hashed when the session stopped, and a request that was never answered.
        let BlockResult::PieceDone(..) = p.on_block(reqs[0].piece, 0, &vec![0; BLOCK_SIZE as usize], peer) else { panic!() };
        let other = p.pick(&[true, true], 1, |_, _| false);
        assert_eq!(other.len(), 1);
        p.reset_transient();
        assert_eq!(p.pick(&[true, true], 5, |_, _| false).len(), 2);
    }
}
