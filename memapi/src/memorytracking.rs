use super::rangemap::RangeMap;
use super::util::new_hashmap;
use ahash::RandomState as ARandomState;
use im::Vector as ImVector;
use inferno::flamegraph;
use itertools::Itertools;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FunctionId(u32);

impl FunctionId {
    pub fn new(id: u32) -> Self {
        FunctionId(id)
    }

    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

/// A function location in the Python source code, e.g. "example() in foo.py".
#[derive(Clone)]
struct FunctionLocation {
    filename: String,
    function_name: String,
}

/// Stores FunctionLocations, returns a FunctionId
#[derive(Clone)]
pub struct FunctionLocations {
    functions: Vec<FunctionLocation>,
}

impl FunctionLocations {
    /// Create a new tracker.
    fn new() -> Self {
        Self {
            functions: Vec::with_capacity(8192),
        }
    }

    /// Register a function, get back its id.
    pub fn add_function(&mut self, filename: String, function_name: String) -> FunctionId {
        self.functions.push(FunctionLocation {
            filename,
            function_name,
        });
        // If we ever have 2 ** 32 or more functions in our program, this will
        // break. Seems unlikely, even with long running workers.
        FunctionId((self.functions.len() - 1) as u32)
    }

    /// Get the function name and filename.
    fn get_function_and_filename(&self, id: FunctionId) -> (&str, &str) {
        let location = &self.functions[id.0 as usize];
        (&location.function_name, &location.filename)
    }
}

/// A specific location: file + function + line number.
#[derive(Clone, Debug, PartialEq, Eq, Copy, Hash)]
pub struct CallSiteId {
    /// The function + filename. We use IDs for performance reasons (faster hashing).
    function: FunctionId,
    /// Line number within the _file_, 1-indexed.
    line_number: u16,
}

impl CallSiteId {
    pub fn new(function: FunctionId, line_number: u16) -> CallSiteId {
        CallSiteId {
            function,
            line_number,
        }
    }
}

/// The current Python callstack.
#[derive(Derivative)]
#[derivative(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Callstack {
    calls: Vec<CallSiteId>,
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    cached_callstack_id: Option<(u16, CallstackId)>, // first bit is line number
}

impl Callstack {
    pub fn new() -> Callstack {
        Callstack {
            calls: Vec::new(),
            cached_callstack_id: None,
        }
    }

    pub fn from_vec(vec: Vec<CallSiteId>) -> Self {
        Self {
            calls: vec,
            cached_callstack_id: None,
        }
    }

    pub fn start_call(&mut self, parent_line_number: u16, callsite_id: CallSiteId) {
        if parent_line_number != 0 {
            if let Some(mut call) = self.calls.last_mut() {
                call.line_number = parent_line_number;
            }
        }
        self.calls.push(callsite_id);
        self.cached_callstack_id = None;
    }

    pub fn finish_call(&mut self) {
        self.calls.pop();
        self.cached_callstack_id = None;
    }

    pub fn id_for_new_allocation<F>(&mut self, line_number: u16, get_callstack_id: F) -> CallstackId
    where
        F: FnOnce(&Callstack) -> CallstackId,
    {
        // If same line number as last callstack, and we have cached callstack
        // ID, reuse it:
        if let Some((previous_line_number, callstack_id)) = self.cached_callstack_id {
            if line_number == previous_line_number {
                return callstack_id;
            }
        }

        // Set the new line number:
        if line_number != 0 {
            if let Some(call) = self.calls.last_mut() {
                call.line_number = line_number;
            }
        }

        // Calculate callstack ID, cache it, and then return it;
        let callstack_id = get_callstack_id(self);
        self.cached_callstack_id = Some((line_number, callstack_id));
        callstack_id
    }

    fn as_string(&self, to_be_post_processed: bool, functions: &FunctionLocations) -> String {
        if self.calls.is_empty() {
            "[No Python stack]".to_string()
        } else {
            self.calls
                .iter()
                .map(|id| {
                    let (function, filename) = functions.get_function_and_filename(id.function);
                    if to_be_post_processed {
                        format!(
                            "{filename}:{line} ({function});TB@@{filename}:{line}@@TB",
                            filename = filename,
                            line = id.line_number,
                            function = function,
                        )
                    } else {
                        format!(
                            "{filename}:{line} ({function})",
                            filename = filename,
                            line = id.line_number,
                            function = function,
                        )
                    }
                })
                .join(";")
        }
    }
}

pub type CallstackId = u32;

/// Maps Functions to integer identifiers used in CallStacks.
struct CallstackInterner {
    max_id: CallstackId,
    callstack_to_id: HashMap<Callstack, u32, ARandomState>,
}

impl<'a> CallstackInterner {
    fn new() -> Self {
        CallstackInterner {
            max_id: 0,
            callstack_to_id: new_hashmap(),
        }
    }

    /// Add a (possibly) new Function, returning its ID.
    fn get_or_insert_id<F: FnOnce() -> ()>(
        &mut self,
        callstack: &Callstack,
        call_on_new: F,
    ) -> CallstackId {
        let max_id = &mut self.max_id;
        if let Some(result) = self.callstack_to_id.get(callstack) {
            *result
        } else {
            let new_id = *max_id;
            *max_id += 1;
            self.callstack_to_id.insert(callstack.clone(), new_id);
            call_on_new();
            new_id
        }
    }

    /// Get map from IDs to Callstacks.
    fn get_reverse_map(&self) -> HashMap<CallstackId, &Callstack, ARandomState> {
        let mut result = new_hashmap();
        for (call_site, csid) in self.callstack_to_id.iter() {
            result.insert(*csid, call_site);
        }
        result
    }
}

const MIB: usize = 1024 * 1024;
const HIGH_32BIT: u32 = 1 << 31;

/// A specific call to malloc()/calloc().
#[derive(Clone, Copy, Debug, PartialEq)]
struct Allocation {
    callstack_id: CallstackId,
    // If high bit is set, this is MiBs (without the high bit being meaningful).
    // Otherwise, it's bytes. We only store MiBs for allocations larger than 2
    // ** 31 bytes (2GB), which means the loss of resolution isn't meaningful.
    // This compression allows us to reduce memory overhead from tracking
    // allocations.
    compressed_size: u32,
}

impl Allocation {
    fn new(callstack_id: CallstackId, size: usize) -> Self {
        let compressed_size = if size >= HIGH_32BIT as usize {
            // Rounding division by MiB, plus the high bit:
            (((size + MIB / 2) / MIB) as u32) | HIGH_32BIT
        } else {
            size as u32
        };
        Allocation {
            callstack_id,
            compressed_size,
        }
    }

    fn size(&self) -> usize {
        if self.compressed_size >= HIGH_32BIT {
            (self.compressed_size - HIGH_32BIT) as usize * MIB
        } else {
            self.compressed_size as usize
        }
    }
}

// Filter down to top 99% of allocated memory.
//
// 1. Empty callstacks are dropped.
// 2. Top 99% of allocations, starting with largest, are kept.
// 3. If that's less than 100 allocations, thrown in up to 100, main goal is
//    just to not have a vast number of useless tiny allocations.
fn filter_to_useful_callstacks(
    allocations: &ImVector<usize>,
) -> HashMap<CallstackId, usize, ARandomState> {
    let total_allocated: usize = allocations.iter().sum();
    let mut stored = 0.0;
    allocations
        .iter()
        // Convert to (callstack id, size) tuples:
        .enumerate()
        // Filter out callstacks with no allocations:
        .filter(|(_, size)| **size > 0)
        // Sort in descending size of allocation:
        .sorted_by(|a, b| Ord::cmp(b.1, a.1))
        // Keep track of how much total allocations we've accumulated so far:
        .map(|(i, size)| {
            stored += *size as f64;
            (stored.clone(), i as u32, size)
        })
        // Stop once we've hit 99% of allocations, but include at least 100 just
        // so there's some context:
        .scan((false, 0), |(past_threshold, taken), (stored, i, size)| {
            if *past_threshold && (*taken > 99) {
                return None;
            }
            *past_threshold = (stored / total_allocated as f64) >= 0.99;
            *taken += 1;
            Some((i, *size))
        })
        .collect()
}

/// The main data structure tracking everything.
pub struct AllocationTracker {
    // malloc()/calloc():
    current_allocations: HashMap<usize, Allocation, ARandomState>,
    // anonymous mmap(), i.e. not file backed:
    current_anon_mmaps: RangeMap<CallstackId>,

    // Map FunctionIds to function + filename strings, so we can store the
    // former and save memory.
    pub functions: FunctionLocations,

    // Map CallstackIds to Callstacks, so we can store the former and save
    // memory:
    interner: CallstackInterner,

    // Both malloc() and mmap():
    current_memory_usage: ImVector<usize>, // Map CallstackId -> total memory usage
    peak_memory_usage: ImVector<usize>,    // Map CallstackId -> total memory usage
    current_allocated_bytes: usize,
    peak_allocated_bytes: usize,
    // Default directory to write out data lacking other info:
    default_path: String,
}

impl<'a> AllocationTracker {
    pub fn new(default_path: String) -> AllocationTracker {
        AllocationTracker {
            current_allocations: new_hashmap(),
            current_anon_mmaps: RangeMap::new(),
            interner: CallstackInterner::new(),
            functions: FunctionLocations::new(),
            current_memory_usage: ImVector::new(),
            peak_memory_usage: ImVector::new(),
            current_allocated_bytes: 0,
            peak_allocated_bytes: 0,
            default_path,
        }
    }

    pub fn get_current_allocated_bytes(&self) -> usize {
        self.current_allocated_bytes
    }

    pub fn get_allocation_size(&self, address: usize) -> usize {
        if let Some(allocation) = self.current_allocations.get(&address) {
            allocation.size()
        } else {
            0
        }
    }

    /// Check if a new peak has been reached:
    fn check_if_new_peak(&mut self) {
        if self.current_allocated_bytes > self.peak_allocated_bytes {
            self.peak_allocated_bytes = self.current_allocated_bytes;
            self.peak_memory_usage
                .clone_from(&self.current_memory_usage);
        }
    }

    fn add_memory_usage(&mut self, callstack_id: CallstackId, bytes: usize) {
        self.current_allocated_bytes += bytes;
        let index = callstack_id as usize;
        self.current_memory_usage[index] += bytes;
    }

    fn remove_memory_usage(&mut self, callstack_id: CallstackId, bytes: usize) {
        self.current_allocated_bytes -= bytes;
        let index = callstack_id as usize;
        // TODO what if goes below zero? add a check I guess, in case of bugs.
        self.current_memory_usage[index] -= bytes;
    }

    pub fn get_callstack_id(&mut self, callstack: &Callstack) -> CallstackId {
        let current_memory_usage = &mut self.current_memory_usage;
        self.interner
            .get_or_insert_id(callstack, || current_memory_usage.push_back(0))
    }

    /// Add a new allocation based off the current callstack.
    pub fn add_allocation(&mut self, address: usize, size: usize, callstack_id: CallstackId) {
        let alloc = Allocation::new(callstack_id, size);
        let compressed_size = alloc.size();
        if let Some(previous) = self.current_allocations.insert(address, alloc) {
            // In production use (proposed commercial product) allocations are
            // only sampled, so missing allocations are common and not the sign
            // of an error.
            #[cfg(not(feature = "production"))]
            {
                // I've seen this happen on macOS only in some threaded code
                // (malloc_on_thread_exit test). Not sure why, but difference was
                // only 16 bytes, which shouldn't have real impact on profiling
                // outcomes.
                eprintln!(
                "=fil-profile= WARNING: Somehow an allocation of size {} disappeared. This can happen if e.g. a library frees memory with private OS APIs. If this happens only a few times with small allocations, it doesn't really matter. If you see this happening a lot, or with large allocations, please file a bug.",
                previous.size()
            );
                // Cleanup the previous allocation, since we never saw its free():
                self.remove_memory_usage(previous.callstack_id, previous.size());
            }
        }
        self.add_memory_usage(callstack_id, compressed_size as usize);
    }

    /// Free an existing allocation.
    pub fn free_allocation(&mut self, address: usize) {
        // Before we reduce memory, let's check if we've previously hit a peak:
        self.check_if_new_peak();

        // Possibly this allocation doesn't exist; that's OK! It can if e.g. we
        // didn't capture an allocation for some reason.
        if let Some(removed) = self.current_allocations.remove(&address) {
            self.remove_memory_usage(removed.callstack_id, removed.size());
        }
    }

    /// Add a new anonymous mmap() based of the current callstack.
    pub fn add_anon_mmap(&mut self, address: usize, size: usize, callstack_id: CallstackId) {
        self.current_anon_mmaps.add(address, size, callstack_id);
        self.add_memory_usage(callstack_id, size);
    }

    pub fn free_anon_mmap(&mut self, address: usize, size: usize) {
        // Before we reduce memory, let's check if we've previously hit a peak:
        self.check_if_new_peak();
        // Now remove, and update totoal memory tracking:
        for (callstack_id, removed) in self.current_anon_mmaps.remove(address, size) {
            self.remove_memory_usage(callstack_id, removed);
        }
    }

    /// Combine Callstacks and make them human-readable. Duplicate callstacks
    /// have their allocated memory summed.
    fn combine_callstacks(
        &mut self,
        // If false, will do the current allocations:
        peak: bool,
    ) -> HashMap<CallstackId, usize, ARandomState> {
        // First, make sure peaks are correct:
        self.check_if_new_peak();

        // Would be nice to validate if data is consistent. However, there are
        // edge cases that make it slightly inconsistent (e.g. see the
        // unexpected code path in add_allocation() above), and blowing up
        // without giving the user their data just because of a small
        // inconsistency doesn't seem ideal. Perhaps if validate() merely
        // reported problems, or maybe validate() should only be enabled in
        // development mode.
        //self.validate();

        // We get a LOT of tiny allocations. To reduce overhead of creating
        // flamegraph (which currently loads EVERYTHING into memory), just do
        // the top 99% of allocations.
        if peak {
            filter_to_useful_callstacks(&self.peak_memory_usage)
        } else {
            filter_to_useful_callstacks(&self.current_memory_usage)
        }
    }

    /// Dump all callstacks in peak memory usage to various files describing the
    /// memory usage.
    pub fn dump_peak_to_flamegraph(&mut self, path: &str) {
        self.dump_to_flamegraph(path, true, "peak-memory", "Peak Tracked Memory Usage", true);
    }

    fn to_lines(
        &mut self,
        peak: bool,
        to_be_post_processed: bool,
    ) -> impl Iterator<Item = String> + '_ {
        let by_call = self.combine_callstacks(peak).into_iter();
        let id_to_callstack = self.interner.get_reverse_map();
        let functions = &self.functions;
        by_call.map(move |(callstack_id, size)| {
            format!(
                "{} {}",
                id_to_callstack
                    .get(&callstack_id)
                    .unwrap()
                    .as_string(to_be_post_processed, functions),
                size,
            )
        })
    }

    fn dump_to_flamegraph(
        &mut self,
        path: &str,
        peak: bool,
        base_filename: &str,
        title: &str,
        to_be_post_processed: bool,
    ) {
        eprintln!("=fil-profile= Preparing to write to {}", path);
        let directory_path = Path::new(path);

        if !directory_path.exists() {
            fs::create_dir_all(directory_path)
                .expect("=fil-profile= Couldn't create the output directory.");
        } else if !directory_path.is_dir() {
            panic!("=fil-profile= Output path must be a directory.");
        }

        let raw_path = directory_path
            .join(format!("{}.prof", base_filename))
            .to_str()
            .unwrap()
            .to_string();

        if let Err(e) = write_lines(self.to_lines(peak, to_be_post_processed), &raw_path) {
            eprintln!("=fil-profile= Error writing raw profiling data: {}", e);
        }
        let svg_path = directory_path
            .join(format!("{}.svg", base_filename))
            .to_str()
            .unwrap()
            .to_string();
        match write_flamegraph(
            &raw_path,
            &svg_path,
            self.peak_allocated_bytes,
            false,
            title,
            to_be_post_processed,
        ) {
            Ok(_) => {
                eprintln!(
                    "=fil-profile= Wrote memory usage flamegraph to {}",
                    svg_path
                );
            }
            Err(e) => {
                eprintln!("=fil-profile= Error writing SVG: {}", e);
            }
        }
        let svg_path = directory_path
            .join(format!("{}-reversed.svg", base_filename))
            .to_str()
            .unwrap()
            .to_string();
        match write_flamegraph(
            &raw_path,
            &svg_path,
            self.peak_allocated_bytes,
            true,
            title,
            to_be_post_processed,
        ) {
            Ok(_) => {
                eprintln!(
                    "=fil-profile= Wrote memory usage flamegraph to {}",
                    svg_path
                );
            }
            Err(e) => {
                eprintln!("=fil-profile= Error writing SVG: {}", e);
            }
        }
    }

    /// Clear memory we won't be needing anymore, since we're going to exit out.
    pub fn oom_break_glass(&mut self) {
        self.current_allocations.clear();
        self.current_allocations.shrink_to_fit();
        self.peak_memory_usage.clear();
    }

    /// Dump information about where we are.
    pub fn oom_dump(&mut self) {
        eprintln!(
            "=fil-profile= We'll try to dump out SVGs. Note that no HTML file will be written."
        );
        let default_path = self.default_path.clone();
        self.dump_to_flamegraph(
            &default_path,
            false,
            "out-of-memory",
            "Current allocations at out-of-memory time",
            false,
        );
        std::process::exit(53);
    }

    /// Validate internal state is in a good state. This won't pass until
    /// check_if_new_peak() is called.
    fn validate(&self) {
        assert!(self.peak_allocated_bytes >= self.current_allocated_bytes);
        let current_allocations = self.current_anon_mmaps.size()
            + self
                .current_allocations
                .iter()
                .map(|(_, alloc)| alloc.size())
                .sum::<usize>();
        assert!(
            current_allocations == self.current_allocated_bytes,
            "{} != {}",
            current_allocations,
            self.current_allocated_bytes
        );
        assert!(self.current_memory_usage.iter().sum::<usize>() == self.current_allocated_bytes);
        assert!(self.peak_memory_usage.iter().sum::<usize>() == self.peak_allocated_bytes);
    }

    /// Reset internal state in way that doesn't invalidate e.g. thread-local
    /// caching of callstack ID.
    pub fn reset(&mut self, default_path: String) {
        self.current_allocations.clear();
        self.current_anon_mmaps = RangeMap::new();
        for i in self.current_memory_usage.iter_mut() {
            *i = 0;
        }
        self.peak_memory_usage = ImVector::new();
        self.current_allocated_bytes = 0;
        self.peak_allocated_bytes = 0;
        self.default_path = default_path;
        self.validate();
    }
}

/// Write strings to disk, one line per string.
fn write_lines<I: Iterator<Item = String>>(lines: I, path: &str) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    for line in lines {
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }
    file.flush()?;
    Ok(())
}

/// Write a flamegraph SVG to disk, given lines in summarized format.
fn write_flamegraph(
    lines_file_path: &str,
    path: &str,
    peak_bytes: usize,
    reversed: bool,
    title: &str,
    to_be_post_processed: bool,
) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    let title = format!(
        "{}{} ({:.1} MiB)",
        title,
        if reversed { ", Reversed" } else { "" },
        peak_bytes as f64 / (1024.0 * 1024.0)
    );
    let mut options = flamegraph::Options {
        title,
        count_name: "bytes".to_string(),
        font_size: 16,
        font_type: "mono".to_string(),
        frame_height: 22,
        reverse_stack_order: reversed,
        color_diffusion: true,
        direction: flamegraph::Direction::Inverted,
        // Maybe disable this some day, but for now it makes debugging much
        // easier:
        pretty_xml: true,
        ..Default::default()
    };
    if to_be_post_processed {
        options.subtitle = Some("SUBTITLE-HERE".to_string());
    }
    if let Err(e) = flamegraph::from_files(&mut options, &[PathBuf::from(lines_file_path)], &file) {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("{}", e),
        ))
    } else {
        file.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        filter_to_useful_callstacks, Allocation, AllocationTracker, CallSiteId, Callstack,
        CallstackInterner, FunctionId, HIGH_32BIT, MIB,
    };
    use im;
    use proptest::prelude::*;
    use std::collections::HashMap;

    proptest! {
        // Allocation sizes smaller than 2 ** 31 are round-tripped.
        #[test]
        fn small_allocation(size in 0..(HIGH_32BIT - 1)) {
            let allocation = Allocation::new(0, size as usize);
            prop_assert_eq!(size as usize, allocation.size());
        }

        // Allocation sizes larger than 2 ** 31 are stored as MiBs, with some
        // loss of resolution.
        #[test]
        fn large_allocation(size in (HIGH_32BIT as usize)..(1 << 50)) {
            let allocation = Allocation::new(0, size as usize);
            let result_size = allocation.size();
            let diff = if size < result_size {
                result_size - size
            } else {
                size - result_size
            };
            prop_assert!(diff <= MIB / 2)
        }

        // Test for https://github.com/pythonspeed/filprofiler/issues/66
        #[test]
        fn correct_allocation_size_tracked(size in (1 as usize)..(1<< 50)) {
            let mut tracker = AllocationTracker::new(".".to_string());
            let cs_id = tracker.get_callstack_id(&Callstack::new());
            tracker.add_allocation(0, size, cs_id);
            tracker.add_anon_mmap(1, size * 2, cs_id);
            // We don't track (large) allocations exactly right, but they should
            // be quite close:
            let ratio = ((size * 3) as f64) / (tracker.current_memory_usage[0] as f64);
            prop_assert!(0.999 < ratio);
            prop_assert!(ratio < 1.001);
            tracker.free_allocation(0);
            tracker.free_anon_mmap(1, size * 2);
            // Once we've freed everything, it should be _exactly_ 0.
            prop_assert_eq!(&im::vector![0], &tracker.current_memory_usage);
            tracker.check_if_new_peak();
            tracker.validate();
        }

        #[test]
        fn current_allocated_matches_sum_of_allocations(
            // Allocated bytes. Will use index as the memory address.
            allocated_sizes in prop::collection::vec(1..1000 as usize, 10..20),
            // Allocations to free.
            free_indices in prop::collection::btree_set(0..10 as usize, 1..5)
        ) {
            let mut tracker = AllocationTracker::new(".".to_string());
            let mut expected_memory_usage = im::vector![];
            for i in 0..allocated_sizes.len() {
                let mut cs = Callstack::new();
                cs.start_call(0, CallSiteId::new(FunctionId::new(i as u32), 0));
                let cs_id = tracker.get_callstack_id(&cs);
                tracker.add_allocation(i as usize,*allocated_sizes.get(i).unwrap(), cs_id);
                expected_memory_usage.push_back(*allocated_sizes.get(i).unwrap());
            }
            let mut expected_sum = allocated_sizes.iter().sum();
            let expected_peak : usize = expected_sum;
            prop_assert_eq!(tracker.current_allocated_bytes, expected_sum);
            prop_assert_eq!(&tracker.current_memory_usage, &expected_memory_usage);
            for i in free_indices.iter() {
                expected_sum -= allocated_sizes.get(*i).unwrap();
                tracker.free_allocation(*i);
                expected_memory_usage[*i] -= allocated_sizes.get(*i).unwrap();
                prop_assert_eq!(tracker.current_allocated_bytes, expected_sum);
                prop_assert_eq!(&tracker.current_memory_usage, &expected_memory_usage);
            }
            prop_assert_eq!(tracker.peak_allocated_bytes, expected_peak);
            tracker.check_if_new_peak();
            tracker.validate();
        }

        #[test]
        fn current_allocated_anon_maps_matches_sum_of_allocations(
            // Allocated bytes. Will use index as the memory address.
            allocated_sizes in prop::collection::vec(1..1000 as usize, 10..20),
            // Allocations to free.
            free_indices in prop::collection::btree_set(0..10 as usize, 1..5)
        ) {
            let mut tracker = AllocationTracker::new(".".to_string());
            let mut expected_memory_usage = im::vector![];
            // Make sure addresses don't overlap:
            let addresses : Vec<usize> = (0..allocated_sizes.len()).map(|i| i * 10000).collect();
            for i in 0..allocated_sizes.len() {
                let mut cs = Callstack::new();
                cs.start_call(0, CallSiteId::new(FunctionId::new(i as u32), 0));
                let csid = tracker.get_callstack_id(&cs);
                tracker.add_anon_mmap(addresses[i] as usize, *allocated_sizes.get(i).unwrap(), csid);
                expected_memory_usage.push_back(*allocated_sizes.get(i).unwrap());
            }
            let mut expected_sum = allocated_sizes.iter().sum();
            let expected_peak : usize = expected_sum;
            prop_assert_eq!(tracker.current_allocated_bytes, expected_sum);
            prop_assert_eq!(&tracker.current_memory_usage, &expected_memory_usage);
            for i in free_indices.iter() {
                expected_sum -= allocated_sizes.get(*i).unwrap();
                tracker.free_anon_mmap(addresses[*i], *allocated_sizes.get(*i).unwrap());
                expected_memory_usage[*i] -= allocated_sizes.get(*i).unwrap();
                prop_assert_eq!(tracker.current_allocated_bytes, expected_sum);
                prop_assert_eq!(&tracker.current_memory_usage, &expected_memory_usage);
            }
            prop_assert_eq!(tracker.peak_allocated_bytes, expected_peak);
            tracker.check_if_new_peak();
            tracker.validate();
        }

        #[test]
        fn filtering_of_callstacks(
            // Allocated bytes. Will use index as the memory address.
            allocated_sizes in prop::collection::vec(1..1000 as usize, 5..5000),
        ) {
            let total_size : usize = allocated_sizes.iter().sum();
            let filtered = filter_to_useful_callstacks(&im::Vector::from(allocated_sizes));
            let filtered_size = filtered.values().into_iter().sum::<usize>() as f64;
            prop_assert!(filtered_size >= 0.99 * (total_size as f64));
            if filtered.len() > 100 {
                // Removing any item should take us below 99%
                for value in filtered.values() {
                    prop_assert!(filtered_size - (*value as f64) < 0.99 * (total_size as f64));
                }
            }
        }
    }

    #[test]
    fn callstack_line_numbers() {
        let fid1 = FunctionId::new(1u32);
        let fid3 = FunctionId::new(3u32);
        let fid5 = FunctionId::new(5u32);

        // Parent line number does nothing if it's first call:
        let mut cs1 = Callstack::new();
        let id1 = CallSiteId::new(fid1, 2);
        let id2 = CallSiteId::new(fid3, 45);
        let id3 = CallSiteId::new(fid5, 6);
        cs1.start_call(123, id1);
        assert_eq!(cs1.calls, vec![id1]);

        // Parent line number does nothing if it's 0:
        cs1.start_call(0, id2);
        assert_eq!(cs1.calls, vec![id1, id2]);

        // Parent line number overrides previous level if it's non-0:
        let mut cs2 = Callstack::new();
        cs2.start_call(0, id1);
        cs2.start_call(10, id2);
        cs2.start_call(12, id3);
        assert_eq!(
            cs2.calls,
            vec![CallSiteId::new(fid1, 10), CallSiteId::new(fid3, 12), id3]
        );
    }

    #[test]
    fn callstackinterner_notices_duplicates() {
        let fid1 = FunctionId::new(1u32);
        let fid3 = FunctionId::new(3u32);

        let mut cs1 = Callstack::new();
        cs1.start_call(0, CallSiteId::new(fid1, 2));
        let cs1b = cs1.clone();
        let mut cs2 = Callstack::new();
        cs2.start_call(0, CallSiteId::new(fid3, 4));
        let cs3 = Callstack::new();
        let cs3b = Callstack::new();

        let mut interner = CallstackInterner::new();

        let mut new = false;
        let id1 = interner.get_or_insert_id(&cs1, || new = true);
        assert!(new);

        new = false;
        let id1b = interner.get_or_insert_id(&cs1b, || new = true);
        assert!(!new);

        new = false;
        let id2 = interner.get_or_insert_id(&cs2, || new = true);
        assert!(new);

        new = false;
        let id3 = interner.get_or_insert_id(&cs3, || new = true);
        assert!(new);

        new = false;
        let id3b = interner.get_or_insert_id(&cs3b, || new = true);
        assert!(!new);

        assert_eq!(id1, id1b);
        assert_ne!(id1, id2);
        assert_ne!(id1, id3);
        assert_ne!(id2, id3);
        assert_eq!(id3, id3b);
        let mut expected = HashMap::default();
        expected.insert(id1, &cs1);
        expected.insert(id2, &cs2);
        expected.insert(id3, &cs3);
        assert_eq!(interner.get_reverse_map(), expected);
    }

    #[test]
    fn callstack_id_for_new_allocation() {
        let mut interner = CallstackInterner::new();

        let mut cs1 = Callstack::new();
        let id0 = cs1.id_for_new_allocation(0, |cs| interner.get_or_insert_id(cs, || ()));
        let id0b = cs1.id_for_new_allocation(0, |cs| interner.get_or_insert_id(cs, || ()));
        assert_eq!(id0, id0b);

        let fid1 = FunctionId::new(1u32);

        cs1.start_call(0, CallSiteId::new(fid1, 2));
        let id1 = cs1.id_for_new_allocation(1, |cs| interner.get_or_insert_id(cs, || ()));
        let id2 = cs1.id_for_new_allocation(2, |cs| interner.get_or_insert_id(cs, || ()));
        let id1b = cs1.id_for_new_allocation(1, |cs| interner.get_or_insert_id(cs, || ()));
        assert_eq!(id1, id1b);
        assert_ne!(id2, id0);
        assert_ne!(id2, id1);

        cs1.start_call(3, CallSiteId::new(fid1, 2));
        let id3 = cs1.id_for_new_allocation(4, |cs| interner.get_or_insert_id(cs, || ()));
        assert_ne!(id3, id0);
        assert_ne!(id3, id1);
        assert_ne!(id3, id2);

        cs1.finish_call();
        let id2b = cs1.id_for_new_allocation(2, |cs| interner.get_or_insert_id(cs, || ()));
        assert_eq!(id2, id2b);
        let id1c = cs1.id_for_new_allocation(1, |cs| interner.get_or_insert_id(cs, || ()));
        assert_eq!(id1, id1c);

        // Check for cache invalidation in start_call:
        cs1.start_call(1, CallSiteId::new(fid1, 1));
        let id4 = cs1.id_for_new_allocation(1, |cs| interner.get_or_insert_id(cs, || ()));
        assert_ne!(id4, id0);
        assert_ne!(id4, id1);
        assert_ne!(id4, id2);
        assert_ne!(id4, id3);

        // Check for cache invalidation in finish_call:
        cs1.finish_call();
        let id1d = cs1.id_for_new_allocation(1, |cs| interner.get_or_insert_id(cs, || ()));
        assert_eq!(id1, id1d);
    }

    #[test]
    fn peak_allocations_only_updated_on_new_peaks() {
        let fid1 = FunctionId::new(1u32);
        let fid3 = FunctionId::new(3u32);

        let mut tracker = AllocationTracker::new(".".to_string());
        let mut cs1 = Callstack::new();
        cs1.start_call(0, CallSiteId::new(fid1, 2));
        let mut cs2 = Callstack::new();
        cs2.start_call(0, CallSiteId::new(fid3, 4));

        let cs1_id = tracker.get_callstack_id(&cs1);

        tracker.add_allocation(1, 1000, cs1_id);
        tracker.check_if_new_peak();
        // Peak should now match current allocations:
        assert_eq!(tracker.current_memory_usage, im::vector![1000]);
        assert_eq!(tracker.current_memory_usage, tracker.peak_memory_usage);
        assert_eq!(tracker.peak_allocated_bytes, 1000);
        let previous_peak = tracker.peak_memory_usage.clone();

        // Free the allocation:
        tracker.free_allocation(1);
        assert_eq!(tracker.current_allocated_bytes, 0);
        assert_eq!(tracker.current_memory_usage, im::vector![0]);
        assert_eq!(previous_peak, tracker.peak_memory_usage);
        assert_eq!(tracker.peak_allocated_bytes, 1000);

        // Add allocation, still less than 1000:
        tracker.add_allocation(3, 123, cs1_id);
        assert_eq!(tracker.current_memory_usage, im::vector![123]);
        tracker.check_if_new_peak();
        assert_eq!(previous_peak, tracker.peak_memory_usage);
        assert_eq!(tracker.peak_allocated_bytes, 1000);

        // Add allocation that goes past previous peak
        let cs2_id = tracker.get_callstack_id(&cs2);
        tracker.add_allocation(2, 2000, cs2_id);
        tracker.check_if_new_peak();
        assert_eq!(tracker.current_memory_usage, im::vector![123, 2000]);
        assert_eq!(tracker.current_memory_usage, tracker.peak_memory_usage);
        assert_eq!(tracker.peak_allocated_bytes, 2123);
        let previous_peak = tracker.peak_memory_usage.clone();

        // Add anonymous mmap() that doesn't go past previous peak:
        tracker.free_allocation(2);
        assert_eq!(tracker.current_memory_usage, im::vector![123, 0]);
        tracker.add_anon_mmap(50000, 1000, cs2_id);
        assert_eq!(tracker.current_memory_usage, im::vector![123, 1000]);
        tracker.check_if_new_peak();
        assert_eq!(tracker.current_allocated_bytes, 1123);
        assert_eq!(tracker.peak_allocated_bytes, 2123);
        assert_eq!(tracker.peak_memory_usage, previous_peak);
        assert_eq!(tracker.current_allocations.len(), 1);
        assert!(tracker.current_allocations.contains_key(&3));
        assert!(tracker.current_anon_mmaps.size() > 0);

        // Add anonymous mmap() that does go past previous peak:
        tracker.add_anon_mmap(600000, 2000, cs2_id);
        assert_eq!(tracker.current_memory_usage, im::vector![123, 3000]);
        tracker.check_if_new_peak();
        assert_eq!(tracker.current_memory_usage, tracker.peak_memory_usage);
        assert_eq!(tracker.current_allocated_bytes, 3123);
        assert_eq!(tracker.peak_allocated_bytes, 3123);

        // Remove mmap():
        tracker.free_anon_mmap(50000, 1000);
        assert_eq!(tracker.current_memory_usage, im::vector![123, 2000]);
        tracker.check_if_new_peak();
        assert_eq!(tracker.current_allocated_bytes, 2123);
        assert_eq!(tracker.peak_allocated_bytes, 3123);
        assert_eq!(tracker.current_anon_mmaps.size(), 2000);
        assert!(tracker
            .current_anon_mmaps
            .as_hashmap()
            .contains_key(&600000));

        // Partial removal of anonmyous mmap():
        tracker.free_anon_mmap(600100, 1000);
        assert_eq!(tracker.current_memory_usage, im::vector![123, 1000]);
        assert_eq!(tracker.current_allocated_bytes, 1123);
        assert_eq!(tracker.peak_allocated_bytes, 3123);
        assert_eq!(tracker.current_anon_mmaps.size(), 1000);
        tracker.check_if_new_peak();
        tracker.validate();
    }

    #[test]
    fn combine_callstacks_and_sum_allocations() {
        let mut tracker = AllocationTracker::new(".".to_string());
        let fid1 = tracker
            .functions
            .add_function("a".to_string(), "af".to_string());
        let fid2 = tracker
            .functions
            .add_function("b".to_string(), "bf".to_string());
        let fid3 = tracker
            .functions
            .add_function("c".to_string(), "cf".to_string());

        let id1 = CallSiteId::new(fid1, 1);
        // Same function, different line number—should be different item:
        let id1_different = CallSiteId::new(fid1, 7);
        let id2 = CallSiteId::new(fid2, 2);

        let id3 = CallSiteId::new(fid3, 3);
        let mut cs1 = Callstack::new();
        cs1.start_call(0, id1);
        cs1.start_call(0, id2.clone());
        let mut cs2 = Callstack::new();
        cs2.start_call(0, id3);
        let mut cs3 = Callstack::new();
        cs3.start_call(0, id1_different);
        cs3.start_call(0, id2);
        let cs1_id = tracker.get_callstack_id(&cs1);
        let cs2_id = tracker.get_callstack_id(&cs2);
        let cs3_id = tracker.get_callstack_id(&cs3);
        tracker.add_allocation(1, 1000, cs1_id);
        tracker.add_allocation(2, 234, cs2_id);
        tracker.add_anon_mmap(3, 50000, cs1_id);
        tracker.add_allocation(4, 6000, cs3_id);

        // 234 allocation is too small, below the 99% total allocations
        // threshold, but we always guarantee at least 100 allocations.
        let mut expected = vec![
            "a:1 (af);TB@@a:1@@TB;b:2 (bf);TB@@b:2@@TB 51000".to_string(),
            "c:3 (cf);TB@@c:3@@TB 234".to_string(),
            "a:7 (af);TB@@a:7@@TB;b:2 (bf);TB@@b:2@@TB 6000".to_string(),
        ];
        let mut result: Vec<String> = tracker.to_lines(true, true).collect();
        result.sort();
        expected.sort();
        assert_eq!(expected, result);

        let mut expected2 = vec![
            "a:1 (af);b:2 (bf) 51000",
            "c:3 (cf) 234",
            "a:7 (af);b:2 (bf) 6000",
        ];
        let mut result2: Vec<String> = tracker.to_lines(true, false).collect();
        result2.sort();
        expected2.sort();
        assert_eq!(expected2, result2);
    }

    // TODO test to_lines(false)
}
