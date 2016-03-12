// Copyright 2015, 2016 Ethcore (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Blockchain database client.

use std::marker::PhantomData;
use std::sync::atomic::AtomicBool;
use util::*;
use util::panics::*;
use views::BlockView;
use error::*;
use header::{BlockNumber};
use state::State;
use spec::Spec;
use engine::Engine;
use views::HeaderView;
use service::{NetSyncMessage, SyncMessage};
use env_info::LastHashes;
use verification::*;
use block::*;
use transaction::LocalizedTransaction;
use extras::TransactionAddress;
use filter::Filter;
use log_entry::LocalizedLogEntry;
use block_queue::{BlockQueue, BlockQueueInfo};
use blockchain::{BlockChain, BlockProvider, TreeRoute};
use client::{BlockId, TransactionId, ClientConfig, BlockChainClient};
pub use blockchain::CacheSize as BlockChainCacheSize;

/// General block status
#[derive(Debug, Eq, PartialEq)]
pub enum BlockStatus {
	/// Part of the blockchain.
	InChain,
	/// Queued for import.
	Queued,
	/// Known as bad.
	Bad,
	/// Unknown.
	Unknown,
}

/// Information about the blockchain gathered together.
#[derive(Debug)]
pub struct BlockChainInfo {
	/// Blockchain difficulty.
	pub total_difficulty: U256,
	/// Block queue difficulty.
	pub pending_total_difficulty: U256,
	/// Genesis block hash.
	pub genesis_hash: H256,
	/// Best blockchain block hash.
	pub best_block_hash: H256,
	/// Best blockchain block number.
	pub best_block_number: BlockNumber
}

impl fmt::Display for BlockChainInfo {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "#{}.{}", self.best_block_number, self.best_block_hash)
	}
}

/// Report on the status of a client.
#[derive(Default, Clone, Debug, Eq, PartialEq)]
pub struct ClientReport {
	/// How many blocks have been imported so far.
	pub blocks_imported: usize,
	/// How many transactions have been applied so far.
	pub transactions_applied: usize,
	/// How much gas has been processed so far.
	pub gas_processed: U256,
	/// Memory used by state DB
	pub state_db_mem: usize,
}

impl ClientReport {
	/// Alter internal reporting to reflect the additional `block` has been processed.
	pub fn accrue_block(&mut self, block: &PreverifiedBlock) {
		self.blocks_imported += 1;
		self.transactions_applied += block.transactions.len();
		self.gas_processed = self.gas_processed + block.header.gas_used;
	}
}

/// Blockchain database client backed by a persistent database. Owns and manages a blockchain and a block queue.
/// Call `import_block()` to import a block asynchronously; `flush_queue()` flushes the queue.
pub struct Client<V = CanonVerifier> where V: Verifier {
	chain: Arc<BlockChain>,
	engine: Arc<Box<Engine>>,
	state_db: Mutex<Box<JournalDB>>,
	block_queue: BlockQueue,
	report: RwLock<ClientReport>,
	import_lock: Mutex<()>,
	panic_handler: Arc<PanicHandler>,

	// for sealing...
	sealing_enabled: AtomicBool,
	sealing_block: Mutex<Option<ClosedBlock>>,
	author: RwLock<Address>,
	extra_data: RwLock<Bytes>,
	verifier: PhantomData<V>,
}

const HISTORY: u64 = 1000;
const CLIENT_DB_VER_STR: &'static str = "5.2";

impl Client<CanonVerifier> {
	/// Create a new client with given spec and DB path.
	pub fn new(config: ClientConfig, spec: Spec, path: &Path, message_channel: IoChannel<NetSyncMessage> ) -> Result<Arc<Client>, Error> {
		Client::<CanonVerifier>::new_with_verifier(config, spec, path, message_channel)
	}
}

impl<V> Client<V> where V: Verifier {
	///  Create a new client with given spec and DB path and custom verifier.
	pub fn new_with_verifier(config: ClientConfig, spec: Spec, path: &Path, message_channel: IoChannel<NetSyncMessage> ) -> Result<Arc<Client<V>>, Error> {
		let mut dir = path.to_path_buf();
		dir.push(H64::from(spec.genesis_header().hash()).hex());
		//TODO: sec/fat: pruned/full versioning
		// version here is a bit useless now, since it's controlled only be the pruning algo.
		dir.push(format!("v{}-sec-{}", CLIENT_DB_VER_STR, config.pruning));
		let path = dir.as_path();
		let gb = spec.genesis_block();
		let chain = Arc::new(BlockChain::new(config.blockchain, &gb, path));
		let mut state_path = path.to_path_buf();
		state_path.push("state");

		let engine = Arc::new(try!(spec.to_engine()));
		let state_path_str = state_path.to_str().unwrap();
		let mut state_db = journaldb::new(state_path_str, config.pruning);

		if state_db.is_empty() && engine.spec().ensure_db_good(state_db.as_hashdb_mut()) {
			state_db.commit(0, &engine.spec().genesis_header().hash(), None).expect("Error commiting genesis state to state DB");
		}

		let block_queue = BlockQueue::new(config.queue, engine.clone(), message_channel);
		let panic_handler = PanicHandler::new_in_arc();
		panic_handler.forward_from(&block_queue);

		Ok(Arc::new(Client {
			chain: chain,
			engine: engine,
			state_db: Mutex::new(state_db),
			block_queue: block_queue,
			report: RwLock::new(Default::default()),
			import_lock: Mutex::new(()),
			panic_handler: panic_handler,
			sealing_enabled: AtomicBool::new(false),
			sealing_block: Mutex::new(None),
			author: RwLock::new(Address::new()),
			extra_data: RwLock::new(Vec::new()),
			verifier: PhantomData,
		}))
	}

	/// Flush the block import queue.
	pub fn flush_queue(&self) {
		self.block_queue.flush();
	}

	fn build_last_hashes(&self, parent_hash: H256) -> LastHashes {
		let mut last_hashes = LastHashes::new();
		last_hashes.resize(256, H256::new());
		last_hashes[0] = parent_hash;
		for i in 0..255 {
			match self.chain.block_details(&last_hashes[i]) {
				Some(details) => {
					last_hashes[i + 1] = details.parent.clone();
				},
				None => break,
			}
		}
		last_hashes
	}

	fn check_and_close_block(&self, block: &PreverifiedBlock) -> Result<ClosedBlock, ()> {
		let engine = self.engine.deref().deref();
		let header = &block.header;

		// Check the block isn't so old we won't be able to enact it.
		let best_block_number = self.chain.best_block_number();
		if best_block_number >= HISTORY && header.number() <= best_block_number - HISTORY {
			warn!(target: "client", "Block import failed for #{} ({})\nBlock is ancient (current best block: #{}).", header.number(), header.hash(), best_block_number);
			return Err(());
		}

		// Verify Block Family
		let verify_family_result = V::verify_block_family(&header, &block.bytes, engine, self.chain.deref());
		if let Err(e) = verify_family_result {
			warn!(target: "client", "Stage 3 block verification failed for #{} ({})\nError: {:?}", header.number(), header.hash(), e);
			return Err(());
		};

		// Check if Parent is in chain
		let chain_has_parent = self.chain.block_header(&header.parent_hash);
		if let None = chain_has_parent {
			warn!(target: "client", "Block import failed for #{} ({}): Parent not found ({}) ", header.number(), header.hash(), header.parent_hash);
			return Err(());
		};

		// Enact Verified Block
		let parent = chain_has_parent.unwrap();
		let last_hashes = self.build_last_hashes(header.parent_hash.clone());
		let db = self.state_db.lock().unwrap().spawn();

		let enact_result = enact_verified(&block, engine, db, &parent, last_hashes);
		if let Err(e) = enact_result {
			warn!(target: "client", "Block import failed for #{} ({})\nError: {:?}", header.number(), header.hash(), e);
			return Err(());
		};

		// Final Verification
		let closed_block = enact_result.unwrap();
		if let Err(e) = V::verify_block_final(&header, closed_block.block().header()) {
			warn!(target: "client", "Stage 4 block verification failed for #{} ({})\nError: {:?}", header.number(), header.hash(), e);
			return Err(());
		}

		Ok(closed_block)
	}

	/// This is triggered by a message coming from a block queue when the block is ready for insertion
	pub fn import_verified_blocks(&self, io: &IoChannel<NetSyncMessage>) -> usize {
		let max_blocks_to_import = 128;

		let mut good_blocks = Vec::with_capacity(max_blocks_to_import);
		let mut bad_blocks = HashSet::new();

		let _import_lock = self.import_lock.lock();
		let blocks = self.block_queue.drain(max_blocks_to_import);

		let original_best = self.chain_info().best_block_hash;

		for block in blocks {
			let header = &block.header;

			if bad_blocks.contains(&header.parent_hash) {
				bad_blocks.insert(header.hash());
				continue;
			}
			let closed_block = self.check_and_close_block(&block);
			if let Err(_) = closed_block {
				bad_blocks.insert(header.hash());
				break;
			}
			good_blocks.push(header.hash());

			// Are we committing an era?
			let ancient = if header.number() >= HISTORY {
				let n = header.number() - HISTORY;
				Some((n, self.chain.block_hash(n).unwrap()))
			} else {
				None
			};

			// Commit results
			let closed_block = closed_block.unwrap();
			let receipts = closed_block.block().receipts().clone();
			closed_block.drain()
				.commit(header.number(), &header.hash(), ancient)
				.expect("State DB commit failed.");

			// And update the chain after commit to prevent race conditions
			// (when something is in chain but you are not able to fetch details)
			self.chain.insert_block(&block.bytes, receipts);

			self.report.write().unwrap().accrue_block(&block);
			trace!(target: "client", "Imported #{} ({})", header.number(), header.hash());
		}

		let imported = good_blocks.len();
		let bad_blocks = bad_blocks.into_iter().collect::<Vec<H256>>();

		{
			if !bad_blocks.is_empty() {
				self.block_queue.mark_as_bad(&bad_blocks);
			}
			if !good_blocks.is_empty() {
				self.block_queue.mark_as_good(&good_blocks);
			}
		}

		{
			if !good_blocks.is_empty() && self.block_queue.queue_info().is_empty() {
				io.send(NetworkIoMessage::User(SyncMessage::NewChainBlocks {
					good: good_blocks,
					bad: bad_blocks,
					// TODO [todr] were to take those from?
					retracted: vec![],
				})).unwrap();
			}
		}

		if self.chain_info().best_block_hash != original_best && self.sealing_enabled.load(atomic::Ordering::Relaxed) {
			self.prepare_sealing();
		}

		imported
	}

	/// Get a copy of the best block's state.
	pub fn state(&self) -> State {
		State::from_existing(self.state_db.lock().unwrap().spawn(), HeaderView::new(&self.best_block_header()).state_root(), self.engine.account_start_nonce())
	}

	/// Get info on the cache.
	pub fn blockchain_cache_info(&self) -> BlockChainCacheSize {
		self.chain.cache_size()
	}

	/// Get the report.
	pub fn report(&self) -> ClientReport {
		let mut report = self.report.read().unwrap().clone();
		report.state_db_mem = self.state_db.lock().unwrap().mem_used();
		report
	}

	/// Tick the client.
	pub fn tick(&self) {
		self.chain.collect_garbage();
		self.block_queue.collect_garbage();
	}

	/// Set up the cache behaviour.
	pub fn configure_cache(&self, pref_cache_size: usize, max_cache_size: usize) {
		self.chain.configure_cache(pref_cache_size, max_cache_size);
	}

	fn block_hash(chain: &BlockChain, id: BlockId) -> Option<H256> {
		match id {
			BlockId::Hash(hash) => Some(hash),
			BlockId::Number(number) => chain.block_hash(number),
			BlockId::Earliest => chain.block_hash(0),
			BlockId::Latest => Some(chain.best_block_hash())
		}
	}

	fn block_number(&self, id: BlockId) -> Option<BlockNumber> {
		match id {
			BlockId::Number(number) => Some(number),
			BlockId::Hash(ref hash) => self.chain.block_number(hash),
			BlockId::Earliest => Some(0),
			BlockId::Latest => Some(self.chain.best_block_number())
		}
	}

	/// Get the author that we will seal blocks as.
	pub fn author(&self) -> Address {
		self.author.read().unwrap().clone()
	}

	/// Set the author that we will seal blocks as.
	pub fn set_author(&self, author: Address) {
		*self.author.write().unwrap() = author;
	}

	/// Get the extra_data that we will seal blocks wuth.
	pub fn extra_data(&self) -> Bytes {
		self.extra_data.read().unwrap().clone()
	}

	/// Set the extra_data that we will seal blocks with.
	pub fn set_extra_data(&self, extra_data: Bytes) {
		*self.extra_data.write().unwrap() = extra_data;
	}

	/// New chain head event. Restart mining operation.
	pub fn prepare_sealing(&self) {
		let h = self.chain.best_block_hash();
		let mut b = OpenBlock::new(
			self.engine.deref().deref(),
			self.state_db.lock().unwrap().spawn(),
			match self.chain.block_header(&h) { Some(ref x) => x, None => {return;} },
			self.build_last_hashes(h.clone()),
			self.author(),
			self.extra_data()
		);

		self.chain.find_uncle_headers(&h, self.engine.deref().deref().maximum_uncle_age()).unwrap().into_iter().take(self.engine.deref().deref().maximum_uncle_count()).foreach(|h| { b.push_uncle(h).unwrap(); });

		// TODO: push transactions.

		let b = b.close();
		trace!("Sealing: number={}, hash={}, diff={}", b.hash(), b.block().header().difficulty(), b.block().header().number());
		*self.sealing_block.lock().unwrap() = Some(b);
	}
}

// TODO: need MinerService MinerIoHandler

impl<V> BlockChainClient for Client<V> where V: Verifier {
	fn block_header(&self, id: BlockId) -> Option<Bytes> {
		Self::block_hash(&self.chain, id).and_then(|hash| self.chain.block(&hash).map(|bytes| BlockView::new(&bytes).rlp().at(0).as_raw().to_vec()))
	}

	fn block_body(&self, id: BlockId) -> Option<Bytes> {
		Self::block_hash(&self.chain, id).and_then(|hash| {
			self.chain.block(&hash).map(|bytes| {
				let rlp = Rlp::new(&bytes);
				let mut body = RlpStream::new_list(2);
				body.append_raw(rlp.at(1).as_raw(), 1);
				body.append_raw(rlp.at(2).as_raw(), 1);
				body.out()
			})
		})
	}

	fn block(&self, id: BlockId) -> Option<Bytes> {
		Self::block_hash(&self.chain, id).and_then(|hash| {
			self.chain.block(&hash)
		})
	}

	fn block_status(&self, id: BlockId) -> BlockStatus {
		match Self::block_hash(&self.chain, id) {
			Some(ref hash) if self.chain.is_known(hash) => BlockStatus::InChain,
			Some(hash) => self.block_queue.block_status(&hash),
			None => BlockStatus::Unknown
		}
	}

	fn block_total_difficulty(&self, id: BlockId) -> Option<U256> {
		Self::block_hash(&self.chain, id).and_then(|hash| self.chain.block_details(&hash)).map(|d| d.total_difficulty)
	}

	fn nonce(&self, address: &Address) -> U256 {
		self.state().nonce(address)
	}

	fn block_hash(&self, id: BlockId) -> Option<H256> {
		Self::block_hash(&self.chain, id)
	}

	fn code(&self, address: &Address) -> Option<Bytes> {
		self.state().code(address)
	}

	fn transaction(&self, id: TransactionId) -> Option<LocalizedTransaction> {
		match id {
			TransactionId::Hash(ref hash) => self.chain.transaction_address(hash),
			TransactionId::Location(id, index) => Self::block_hash(&self.chain, id).map(|hash| TransactionAddress {
				block_hash: hash,
				index: index
			})
		}.and_then(|address| self.chain.transaction(&address))
	}

	fn tree_route(&self, from: &H256, to: &H256) -> Option<TreeRoute> {
		match self.chain.is_known(from) && self.chain.is_known(to) {
			true => Some(self.chain.tree_route(from.clone(), to.clone())),
			false => None
		}
	}

	fn state_data(&self, _hash: &H256) -> Option<Bytes> {
		None
	}

	fn block_receipts(&self, _hash: &H256) -> Option<Bytes> {
		None
	}

	fn import_block(&self, bytes: Bytes) -> ImportResult {
		{
			let header = BlockView::new(&bytes).header_view();
			if self.chain.is_known(&header.sha3()) {
				return Err(x!(ImportError::AlreadyInChain));
			}
			if self.block_status(BlockId::Hash(header.parent_hash())) == BlockStatus::Unknown {
				return Err(x!(BlockError::UnknownParent(header.parent_hash())));
			}
		}
		self.block_queue.import_block(bytes)
	}

	fn queue_info(&self) -> BlockQueueInfo {
		self.block_queue.queue_info()
	}

	fn clear_queue(&self) {
		self.block_queue.clear();
	}

	fn chain_info(&self) -> BlockChainInfo {
		BlockChainInfo {
			total_difficulty: self.chain.best_block_total_difficulty(),
			pending_total_difficulty: self.chain.best_block_total_difficulty(),
			genesis_hash: self.chain.genesis_hash(),
			best_block_hash: self.chain.best_block_hash(),
			best_block_number: From::from(self.chain.best_block_number())
		}
	}

	fn blocks_with_bloom(&self, bloom: &H2048, from_block: BlockId, to_block: BlockId) -> Option<Vec<BlockNumber>> {
		match (self.block_number(from_block), self.block_number(to_block)) {
			(Some(from), Some(to)) => Some(self.chain.blocks_with_bloom(bloom, from, to)),
			_ => None
		}
	}

	fn logs(&self, filter: Filter) -> Vec<LocalizedLogEntry> {
		// TODO: lock blockchain only once

		let mut blocks = filter.bloom_possibilities().iter()
			.filter_map(|bloom| self.blocks_with_bloom(bloom, filter.from_block.clone(), filter.to_block.clone()))
			.flat_map(|m| m)
			// remove duplicate elements
			.collect::<HashSet<u64>>()
			.into_iter()
			.collect::<Vec<u64>>();

		blocks.sort();

		blocks.into_iter()
			.filter_map(|number| self.chain.block_hash(number).map(|hash| (number, hash)))
			.filter_map(|(number, hash)| self.chain.block_receipts(&hash).map(|r| (number, hash, r.receipts)))
			.filter_map(|(number, hash, receipts)| self.chain.block(&hash).map(|ref b| (number, hash, receipts, BlockView::new(b).transaction_hashes())))
			.flat_map(|(number, hash, receipts, hashes)| {
				let mut log_index = 0;
				receipts.into_iter()
					.enumerate()
					.flat_map(|(index, receipt)| {
						log_index += receipt.logs.len();
						receipt.logs.into_iter()
							.enumerate()
							.filter(|tuple| filter.matches(&tuple.1))
						 	.map(|(i, log)| LocalizedLogEntry {
							 	entry: log,
								block_hash: hash.clone(),
								block_number: number as usize,
								transaction_hash: hashes.get(index).cloned().unwrap_or_else(H256::new),
								transaction_index: index,
								log_index: log_index + i
							})
							.collect::<Vec<LocalizedLogEntry>>()
					})
					.collect::<Vec<LocalizedLogEntry>>()

			})
			.collect()
	}

	/// Grab the `ClosedBlock` that we want to be sealed. Comes as a mutex that you have to lock.
	fn sealing_block(&self) -> &Mutex<Option<ClosedBlock>> {
		if self.sealing_block.lock().unwrap().is_none() {
			self.sealing_enabled.store(true, atomic::Ordering::Relaxed);
			// TODO: Above should be on a timer that resets after two blocks have arrived without being asked for.
			self.prepare_sealing();
		}
		&self.sealing_block
	}

	/// Submit `seal` as a valid solution for the header of `pow_hash`.
	/// Will check the seal, but not actually insert the block into the chain.
	fn submit_seal(&self, pow_hash: H256, seal: Vec<Bytes>) -> Result<(), Error> {
		let mut maybe_b = self.sealing_block.lock().unwrap();
		match *maybe_b {
			Some(ref b) if b.hash() == pow_hash => {}
			_ => { return Err(Error::PowHashInvalid); }
		}

		let b = maybe_b.take();
		match b.unwrap().try_seal(self.engine.deref().deref(), seal) {
			Err(old) => {
				*maybe_b = Some(old);
				Err(Error::PowInvalid)
			}
			Ok(sealed) => {
				// TODO: commit DB from `sealed.drain` and make a VerifiedBlock to skip running the transactions twice.
				try!(self.import_block(sealed.rlp_bytes()));
				Ok(())
			}
		}
	}
}

impl MayPanic for Client {
	fn on_panic<F>(&self, closure: F) where F: OnPanicListener {
		self.panic_handler.on_panic(closure);
	}
}