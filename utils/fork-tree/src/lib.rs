// This file is part of Substrate.

// Copyright (C) 2019-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Utility library for managing tree-like ordered data with logic for pruning
//! the tree while finalizing nodes.

#![warn(missing_docs)]

use std::cmp::Reverse;
use std::fmt;
use codec::{Decode, Encode};

/// Error occurred when iterating with the tree.
#[derive(Clone, Debug, PartialEq)]
pub enum Error<E> {
	/// Adding duplicate node to tree.
	Duplicate,
	/// Finalizing descendent of tree node without finalizing ancestor(s).
	UnfinalizedAncestor,
	/// Imported or finalized node that is an ancestor of previously finalized node.
	Revert,
	/// Error throw by client when checking for node ancestry.
	Client(E),
}

impl<E: std::error::Error> fmt::Display for Error<E> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		let message = match *self {
			Error::Duplicate => "Hash already exists in Tree".into(),
			Error::UnfinalizedAncestor => "Finalized descendent of Tree node without finalizing its ancestor(s) first".into(),
			Error::Revert => "Tried to import or finalize node that is an ancestor of a previously finalized node".into(),
			Error::Client(ref err) => format!("Client error: {}", err),
		};
		write!(f, "{}", message)
	}
}

impl<E: std::error::Error> std::error::Error for Error<E> {
	fn cause(&self) -> Option<&dyn std::error::Error> {
		None
	}
}

impl<E: std::error::Error> From<E> for Error<E> {
	fn from(err: E) -> Error<E> {
		Error::Client(err)
	}
}

/// Result of finalizing a node (that could be a part of the tree or not).
#[derive(Debug, PartialEq)]
pub enum FinalizationResult<V> {
	/// The tree has changed, optionally return the value associated with the finalized node.
	Changed(Option<V>),
	/// The tree has not changed.
	Unchanged,
}

/// A tree data structure that stores several nodes across multiple branches.
/// Top-level branches are called roots. The tree has functionality for
/// finalizing nodes, which means that that node is traversed, and all competing
/// branches are pruned. It also guarantees that nodes in the tree are finalized
/// in order. Each node is uniquely identified by its hash but can be ordered by
/// its number. In order to build the tree an external function must be provided
/// when interacting with the tree to establish a node's ancestry.
#[derive(Clone, Debug, Decode, Encode, PartialEq)]
pub struct ForkTree<H, N, V> {
	roots: Vec<Node<H, N, V>>,
	best_finalized_number: Option<N>,
}

impl<H, N, V> ForkTree<H, N, V> where
	H: PartialEq + Clone,
	N: Ord + Clone,
	V: Clone,
{
	/// Prune the tree, removing all non-canonical nodes. We find the node in the
	/// tree that is the deepest ancestor of the given hash and that passes the
	/// given predicate. If such a node exists, we re-root the tree to this
	/// node. Otherwise the tree remains unchanged. The given function
	/// `is_descendent_of` should return `true` if the second hash (target) is a
	/// descendent of the first hash (base).
	///
	/// Returns all pruned node data.
	pub fn prune<F, E, P>(
		&mut self,
		hash: &H,
		number: &N,
		is_descendent_of: &F,
		predicate: &P,
	) -> Result<impl Iterator<Item=(H, N, V)>, Error<E>>
		where E: std::error::Error,
			  F: Fn(&H, &H) -> Result<bool, E>,
			  P: Fn(&V) -> bool,
	{
		let new_root_index = self.find_node_index_where(
			hash,
			number,
			is_descendent_of,
			predicate,
		)?;

		let removed = if let Some(mut root_index) = new_root_index {
			let mut old_roots = std::mem::take(&mut self.roots);

			let mut root = None;
			let mut cur_children = Some(&mut old_roots);

			while let Some(cur_index) = root_index.pop() {
				if let Some(children) = cur_children.take() {
					if root_index.is_empty() {
						root = Some(children.remove(cur_index));
					} else {
						cur_children = Some(&mut children[cur_index].children);
					}
				}
			}

			let mut root = root
				.expect("find_node_index_where will return array with at least one index; \
						 this results in at least one item in removed; qed");

			let mut removed = old_roots;

			// we found the deepest ancestor of the finalized block, so we prune
			// out any children that don't include the finalized block.
			let root_children = std::mem::take(&mut root.children);
			let mut is_first = true;

			for child in root_children {
				if is_first &&
					(child.number == *number && child.hash == *hash ||
					 child.number < *number && is_descendent_of(&child.hash, hash).unwrap_or(false))
				{
					root.children.push(child);
					// assuming that the tree is well formed only one child should pass this requirement
					// due to ancestry restrictions (i.e. they must be different forks).
					is_first = false;
				} else {
					removed.push(child);
				}
			}

			self.roots = vec![root];

			removed
		} else {
			Vec::new()
		};

		self.rebalance();

		Ok(RemovedIterator { stack: removed })
	}
}

impl<H, N, V> ForkTree<H, N, V> where
	H: PartialEq,
	N: Ord,
{
	/// Create a new empty tree.
	pub fn new() -> ForkTree<H, N, V> {
		ForkTree {
			roots: Vec::new(),
			best_finalized_number: None,
		}
	}

	/// Rebalance the tree, i.e. sort child nodes by max branch depth
	/// (decreasing).
	///
	/// Most operations in the tree are performed with depth-first search
	/// starting from the leftmost node at every level, since this tree is meant
	/// to be used in a blockchain context, a good heuristic is that the node
	/// we'll be looking
	/// for at any point will likely be in one of the deepest chains (i.e. the
	/// longest ones).
	pub fn rebalance(&mut self) {
		self.roots.sort_by_key(|n| Reverse(n.max_depth()));
		for root in &mut self.roots {
			root.rebalance();
		}
	}

	/// Import a new node into the tree. The given function `is_descendent_of`
	/// should return `true` if the second hash (target) is a descendent of the
	/// first hash (base). This method assumes that nodes in the same branch are
	/// imported in order.
	///
	/// Returns `true` if the imported node is a root.
	pub fn import<F, E>(
		&mut self,
		mut hash: H,
		mut number: N,
		mut data: V,
		is_descendent_of: &F,
	) -> Result<bool, Error<E>>
		where E: std::error::Error,
			  F: Fn(&H, &H) -> Result<bool, E>,
	{
		if let Some(ref best_finalized_number) = self.best_finalized_number {
			if number <= *best_finalized_number {
				return Err(Error::Revert);
			}
		}

		for root in self.roots.iter_mut() {
			if root.hash == hash {
				return Err(Error::Duplicate);
			}

			match root.import(hash, number, data, is_descendent_of)? {
				Some((h, n, d)) => {
					hash = h;
					number = n;
					data = d;
				},
				None => return Ok(false),
			}
		}

		self.roots.push(Node {
			data,
			hash: hash,
			number: number,
			children: Vec::new(),
		});

		self.rebalance();

		Ok(true)
	}

	/// Iterates over the existing roots in the tree.
	pub fn roots(&self) -> impl Iterator<Item=(&H, &N, &V)> {
		self.roots.iter().map(|node| (&node.hash, &node.number, &node.data))
	}

	fn node_iter(&self) -> impl Iterator<Item=&Node<H, N, V>> {
		ForkTreeIterator { stack: self.roots.iter().collect() }
	}

	/// Iterates the nodes in the tree in pre-order.
	pub fn iter(&self) -> impl Iterator<Item=(&H, &N, &V)> {
		self.node_iter().map(|node| (&node.hash, &node.number, &node.data))
	}

	/// Find a node in the tree that is the deepest ancestor of the given
	/// block hash and which passes the given predicate. The given function
	/// `is_descendent_of` should return `true` if the second hash (target)
	/// is a descendent of the first hash (base).
	pub fn find_node_where<F, E, P>(
		&self,
		hash: &H,
		number: &N,
		is_descendent_of: &F,
		predicate: &P,
	) -> Result<Option<&Node<H, N, V>>, Error<E>> where
		E: std::error::Error,
		F: Fn(&H, &H) -> Result<bool, E>,
		P: Fn(&V) -> bool,
	{
		// search for node starting from all roots
		for root in self.roots.iter() {
			let node = root.find_node_where(hash, number, is_descendent_of, predicate)?;

			// found the node, early exit
			if let FindOutcome::Found(node) = node {
				return Ok(Some(node));
			}
		}

		Ok(None)
	}

	/// Map fork tree into values of new types.
	pub fn map<VT, F>(
		self,
		f: &mut F,
	) -> ForkTree<H, N, VT> where
		F: FnMut(&H, &N, V) -> VT,
	{
		let roots = self.roots
			.into_iter()
			.map(|root| {
				root.map(f)
			})
			.collect();

		ForkTree {
			roots,
			best_finalized_number: self.best_finalized_number,
		}
	}

	/// Same as [`find_node_where`](ForkTree::find_node_where), but returns mutable reference.
	pub fn find_node_where_mut<F, E, P>(
		&mut self,
		hash: &H,
		number: &N,
		is_descendent_of: &F,
		predicate: &P,
	) -> Result<Option<&mut Node<H, N, V>>, Error<E>> where
		E: std::error::Error,
		F: Fn(&H, &H) -> Result<bool, E>,
		P: Fn(&V) -> bool,
	{
		// search for node starting from all roots
		for root in self.roots.iter_mut() {
			let node = root.find_node_where_mut(hash, number, is_descendent_of, predicate)?;

			// found the node, early exit
			if let FindOutcome::Found(node) = node {
				return Ok(Some(node));
			}
		}

		Ok(None)
	}

	/// Same as [`find_node_where`](ForkTree::find_node_where), but returns indexes.
	pub fn find_node_index_where<F, E, P>(
		&self,
		hash: &H,
		number: &N,
		is_descendent_of: &F,
		predicate: &P,
	) -> Result<Option<Vec<usize>>, Error<E>> where
		E: std::error::Error,
		F: Fn(&H, &H) -> Result<bool, E>,
		P: Fn(&V) -> bool,
	{
		// search for node starting from all roots
		for (index, root) in self.roots.iter().enumerate() {
			let node = root.find_node_index_where(hash, number, is_descendent_of, predicate)?;

			// found the node, early exit
			if let FindOutcome::Found(mut node) = node {
				node.push(index);
				return Ok(Some(node));
			}
		}

		Ok(None)
	}

	/// Finalize a root in the tree and return it, return `None` in case no root
	/// with the given hash exists. All other roots are pruned, and the children
	/// of the finalized node become the new roots.
	pub fn finalize_root(&mut self, hash: &H) -> Option<V> {
		self.roots.iter().position(|node| node.hash == *hash)
			.map(|position| self.finalize_root_at(position))
	}

	/// Finalize root at given position. See `finalize_root` comment for details.
	fn finalize_root_at(&mut self, position: usize) -> V {
		let node = self.roots.swap_remove(position);
		self.roots = node.children;
		self.best_finalized_number = Some(node.number);
		return node.data;
	}

	/// Finalize a node in the tree. This method will make sure that the node
	/// being finalized is either an existing root (and return its data), or a
	/// node from a competing branch (not in the tree), tree pruning is done
	/// accordingly. The given function `is_descendent_of` should return `true`
	/// if the second hash (target) is a descendent of the first hash (base).
	pub fn finalize<F, E>(
		&mut self,
		hash: &H,
		number: N,
		is_descendent_of: &F,
	) -> Result<FinalizationResult<V>, Error<E>>
		where E: std::error::Error,
			  F: Fn(&H, &H) -> Result<bool, E>
	{
		if let Some(ref best_finalized_number) = self.best_finalized_number {
			if number <= *best_finalized_number {
				return Err(Error::Revert);
			}
		}

		// check if one of the current roots is being finalized
		if let Some(root) = self.finalize_root(hash) {
			return Ok(FinalizationResult::Changed(Some(root)));
		}

		// make sure we're not finalizing a descendent of any root
		for root in self.roots.iter() {
			if number > root.number && is_descendent_of(&root.hash, hash)? {
				return Err(Error::UnfinalizedAncestor);
			}
		}

		// we finalized a block earlier than any existing root (or possibly
		// another fork not part of the tree). make sure to only keep roots that
		// are part of the finalized branch
		let mut changed = false;
		self.roots.retain(|root| {
			let retain = root.number > number && is_descendent_of(hash, &root.hash).unwrap_or(false);

			if !retain {
				changed = true;
			}

			retain
		});

		self.best_finalized_number = Some(number);

		if changed {
			Ok(FinalizationResult::Changed(None))
		} else {
			Ok(FinalizationResult::Unchanged)
		}
	}

	/// Finalize a node in the tree and all its ancestors. The given function
	/// `is_descendent_of` should return `true` if the second hash (target) is
	// a descendent of the first hash (base).
	pub fn finalize_with_ancestors<F, E>(
		&mut self,
		hash: &H,
		number: N,
		is_descendent_of: &F,
	) -> Result<FinalizationResult<V>, Error<E>>
		where E: std::error::Error,
				F: Fn(&H, &H) -> Result<bool, E>
	{
		if let Some(ref best_finalized_number) = self.best_finalized_number {
			if number <= *best_finalized_number {
				return Err(Error::Revert);
			}
		}

		// check if one of the current roots is being finalized
		if let Some(root) = self.finalize_root(hash) {
			return Ok(FinalizationResult::Changed(Some(root)));
		}

		// we need to:
		// 1) remove all roots that are not ancestors AND not descendants of finalized block;
		// 2) if node is descendant - just leave it;
		// 3) if node is ancestor - 'open it'
		let mut changed = false;
		let mut idx = 0;
		while idx != self.roots.len() {
			let (is_finalized, is_descendant, is_ancestor) = {
				let root = &self.roots[idx];
				let is_finalized = root.hash == *hash;
				let is_descendant = !is_finalized
					&& root.number > number && is_descendent_of(hash, &root.hash).unwrap_or(false);
				let is_ancestor = !is_finalized && !is_descendant
					&& root.number < number && is_descendent_of(&root.hash, hash).unwrap_or(false);
				(is_finalized, is_descendant, is_ancestor)
			};

			// if we have met finalized root - open it and return
			if is_finalized {
				return Ok(FinalizationResult::Changed(Some(self.finalize_root_at(idx))));
			}

			// if node is descendant of finalized block - just leave it as is
			if is_descendant {
				idx += 1;
				continue;
			}

			// if node is ancestor of finalized block - remove it and continue with children
			if is_ancestor {
				let root = self.roots.swap_remove(idx);
				self.roots.extend(root.children);
				changed = true;
				continue;
			}

			// if node is neither ancestor, nor descendant of the finalized block - remove it
			self.roots.swap_remove(idx);
			changed = true;
		}

		self.best_finalized_number = Some(number);

		if changed {
			Ok(FinalizationResult::Changed(None))
		} else {
			Ok(FinalizationResult::Unchanged)
		}
	}

	/// Checks if any node in the tree is finalized by either finalizing the
	/// node itself or a child node that's not in the tree, guaranteeing that
	/// the node being finalized isn't a descendent of any of the node's
	/// children. Returns `Some(true)` if the node being finalized is a root,
	/// `Some(false)` if the node being finalized is not a root, and `None` if
	/// no node in the tree is finalized. The given `predicate` is checked on
	/// the prospective finalized root and must pass for finalization to occur.
	/// The given function `is_descendent_of` should return `true` if the second
	/// hash (target) is a descendent of the first hash (base).
	pub fn finalizes_any_with_descendent_if<F, P, E>(
		&self,
		hash: &H,
		number: N,
		is_descendent_of: &F,
		predicate: P,
	) -> Result<Option<bool>, Error<E>>
		where E: std::error::Error,
			  F: Fn(&H, &H) -> Result<bool, E>,
			  P: Fn(&V) -> bool,
	{
		if let Some(ref best_finalized_number) = self.best_finalized_number {
			if number <= *best_finalized_number {
				return Err(Error::Revert);
			}
		}

		// check if the given hash is equal or a descendent of any node in the
		// tree, if we find a valid node that passes the predicate then we must
		// ensure that we're not finalizing past any of its child nodes.
		for node in self.node_iter() {
			if predicate(&node.data) {
				if node.hash == *hash || is_descendent_of(&node.hash, hash)? {
					for node in node.children.iter() {
						if node.number <= number && is_descendent_of(&node.hash, &hash)? {
							return Err(Error::UnfinalizedAncestor);
						}
					}

					return Ok(Some(self.roots.iter().any(|root| root.hash == node.hash)));
				}
			}
		}

		Ok(None)
	}

	/// Finalize a root in the tree by either finalizing the node itself or a
	/// child node that's not in the tree, guaranteeing that the node being
	/// finalized isn't a descendent of any of the root's children. The given
	/// `predicate` is checked on the prospective finalized root and must pass for
	/// finalization to occur. The given function `is_descendent_of` should
	/// return `true` if the second hash (target) is a descendent of the first
	/// hash (base).
	pub fn finalize_with_descendent_if<F, P, E>(
		&mut self,
		hash: &H,
		number: N,
		is_descendent_of: &F,
		predicate: P,
	) -> Result<FinalizationResult<V>, Error<E>>
		where E: std::error::Error,
			  F: Fn(&H, &H) -> Result<bool, E>,
			  P: Fn(&V) -> bool,
	{
		if let Some(ref best_finalized_number) = self.best_finalized_number {
			if number <= *best_finalized_number {
				return Err(Error::Revert);
			}
		}

		// check if the given hash is equal or a a descendent of any root, if we
		// find a valid root that passes the predicate then we must ensure that
		// we're not finalizing past any children node.
		let mut position = None;
		for (i, root) in self.roots.iter().enumerate() {
			if predicate(&root.data) {
				if root.hash == *hash || is_descendent_of(&root.hash, hash)? {
					for node in root.children.iter() {
						if node.number <= number && is_descendent_of(&node.hash, &hash)? {
							return Err(Error::UnfinalizedAncestor);
						}
					}

					position = Some(i);
					break;
				}
			}
		}

		let node_data = position.map(|i| {
			let node = self.roots.swap_remove(i);
			self.roots = node.children;
			self.best_finalized_number = Some(node.number);
			node.data
		});

		// if the block being finalized is earlier than a given root, then it
		// must be its ancestor, otherwise we can prune the root. if there's a
		// root at the same height then the hashes must match. otherwise the
		// node being finalized is higher than the root so it must be its
		// descendent (in this case the node wasn't finalized earlier presumably
		// because the predicate didn't pass).
		let mut changed = false;
		self.roots.retain(|root| {
			let retain =
				root.number > number && is_descendent_of(hash, &root.hash).unwrap_or(false) ||
				root.number == number && root.hash == *hash ||
				is_descendent_of(&root.hash, hash).unwrap_or(false);

			if !retain {
				changed = true;
			}

			retain
		});

		self.best_finalized_number = Some(number);

		match (node_data, changed) {
			(Some(data), _) => Ok(FinalizationResult::Changed(Some(data))),
			(None, true) => Ok(FinalizationResult::Changed(None)),
			(None, false) => Ok(FinalizationResult::Unchanged),
		}
	}
}

// Workaround for: https://github.com/rust-lang/rust/issues/34537
mod node_implementation {
	use super::*;

	/// The outcome of a search within a node.
	pub enum FindOutcome<T> {
		// this is the node we were looking for.
		Found(T),
		// not the node we're looking for. contains a flag indicating
		// whether the node was a descendent. true implies the predicate failed.
		Failure(bool),
		// Abort search.
		Abort,
	}

	#[derive(Clone, Debug, Decode, Encode, PartialEq)]
	pub struct Node<H, N, V> {
		pub hash: H,
		pub number: N,
		pub data: V,
		pub children: Vec<Node<H, N, V>>,
	}

	impl<H: PartialEq, N: Ord, V> Node<H, N, V> {
		/// Rebalance the tree, i.e. sort child nodes by max branch depth (decreasing).
		pub fn rebalance(&mut self) {
			let mut stack: Vec<(*mut Self, usize)> = Vec::new();
			stack.push((self as *mut _, 0));
			loop {
				let child_pointer = if let Some(last) = stack.last_mut() {
					let node: &mut Self = unsafe { last.0.as_mut().unwrap() };
					node.children.sort_by_key(|n| Reverse(n.max_depth()));
					if last.1 < node.children.len() {
						last.1 += 1;
						Some(&mut node.children[last.1 - 1] as *mut _)
					} else {
						// pop
						None
					}
				} else {
					break;
				};
				if let Some(child) = child_pointer {
					stack.push((child, 0));
				} else {
					let _ = stack.pop();
				}
			}
		}

		/// Finds the max depth among all branches descendent from this node.
		pub fn max_depth(&self) -> usize {
			let mut max = 0;

			for node in &self.children {
				max = node.max_depth().max(max)
			}

			max + 1
		}

		/// Map node data into values of new types.
		pub fn map<VT, F>(
			self,
			f: &mut F,
		) -> Node<H, N, VT> where
			F: FnMut(&H, &N, V) -> VT,
		{
			let children = self.children
				.into_iter()
				.map(|node| {
					node.map(f)
				})
				.collect();

			let vt = f(&self.hash, &self.number, self.data);
			Node {
				hash: self.hash,
				number: self.number,
				data: vt,
				children,
			}
		}

		pub fn import<F, E: std::error::Error>(
			&mut self,
			hash: H,
			number: N,
			data: V,
			is_descendent_of: &F,
		) -> Result<Option<(H, N, V)>, Error<E>>
			where E: fmt::Debug,
				  F: Fn(&H, &H) -> Result<bool, E>,
		{
			if self.hash == hash {
				return Err(Error::Duplicate);
			};

			let mut stack: Vec<(*mut Self, usize)> = Vec::new();
			stack.push((self as *mut _, 0));

			loop {
				let child_pointer = if let Some(last) = stack.last_mut() {
					let node: &mut Self = unsafe { last.0.as_mut().unwrap() };
					if node.hash == hash {
						return Err(Error::Duplicate);
					};


					if number <= node.number {
						None
					} else {
						if last.1 < node.children.len() {
							last.1 += 1;
							Some(&mut node.children[last.1 - 1] as *mut _)
						} else {
							// pop
							None
						}
					}
				} else {
					break;
				};

				if let Some(child) = child_pointer {
					stack.push((child, 0));
				} else {
					if let Some(last) = stack.pop() {
						let node: &mut Self = unsafe { last.0.as_mut().unwrap() };
						if is_descendent_of(&node.hash, &hash)? {
							node.children.push(Node {
								data,
								hash: hash,
								number: number,
								children: Vec::new(),
							});

							return Ok(None);
						}
					}
				}
			}

			Ok(Some((hash, number, data)))
		}

		/// Find a node in the tree that is the deepest ancestor of the given
		/// block hash which also passes the given predicate, backtracking
		/// when the predicate fails.
		/// The given function `is_descendent_of` should return `true` if the second hash (target)
		/// is a descendent of the first hash (base).
		///
		/// The returned indices are from last to first. The earliest index in the traverse path
		/// goes last, and the final index in the traverse path goes first. An empty list means
		/// that the current node is the result.
		pub fn find_node_index_where<F, P, E>(
			&self,
			hash: &H,
			number: &N,
			is_descendent_of: &F,
			predicate: &P,
		) -> Result<FindOutcome<Vec<usize>>, Error<E>>
			where E: std::error::Error,
				  F: Fn(&H, &H) -> Result<bool, E>,
				  P: Fn(&V) -> bool,
		{
			if *number < self.number {
				return Ok(FindOutcome::Failure(false));
			}

			let mut stack: Vec<(&Self, usize)> = Vec::new();
			stack.push((self, 0));

			let mut found = false;
			let mut touched_descendant = false;
			loop {
				let descend_node = if touched_descendant {
					None
				} else if let Some(last) = stack.last_mut() {
					let node: &Self = last.0;
					// Don't search children
					if *number <= node.number {
						None
					} else {
						if last.1 < node.children.len() {
							last.1 += 1;
							Some(&node.children[last.1 - 1])
						} else {
							// pop
							None
						}
					}
				} else {
					break;
				};


				if let Some(child) = descend_node {
					stack.push((child, 0));
				} else {
					if let Some(last) = stack.pop() {
						let node: &Self = &last.0;
						if touched_descendant || is_descendent_of(&node.hash, &hash)? {
							// if the predicate passes we return the node
							if predicate(&node.data) {
								found = true;
								break;
							}
							touched_descendant = true;
						}
					} else {
						break;
					}
				}
			}

			if found {
				let path: Vec<usize> = stack.iter().rev().map(|item| item.1 - 1).collect();
				Ok(FindOutcome::Found(path))
			} else {
				Ok(FindOutcome::Failure(touched_descendant))
			}
		}

		/// Find a node in the tree that is the deepest ancestor of the given
		/// block hash which also passes the given predicate, backtracking
		/// when the predicate fails.
		/// The given function `is_descendent_of` should return `true` if the second hash (target)
		/// is a descendent of the first hash (base).
		pub fn find_node_where<F, P, E>(
			&self,
			hash: &H,
			number: &N,
			is_descendent_of: &F,
			predicate: &P,
		) -> Result<FindOutcome<&Node<H, N, V>>, Error<E>>
			where E: std::error::Error,
				  F: Fn(&H, &H) -> Result<bool, E>,
				  P: Fn(&V) -> bool,
		{
			let outcome = self.find_node_index_where(hash, number, is_descendent_of, predicate)?;

			match outcome {
				FindOutcome::Abort => Ok(FindOutcome::Abort),
				FindOutcome::Failure(f) => Ok(FindOutcome::Failure(f)),
				FindOutcome::Found(mut indexes) => {
					let mut cur = self;

					while let Some(i) = indexes.pop() {
						cur = &cur.children[i];
					}
					Ok(FindOutcome::Found(cur))
				},
			}
		}

		/// Find a node in the tree that is the deepest ancestor of the given
		/// block hash which also passes the given predicate, backtracking
		/// when the predicate fails.
		/// The given function `is_descendent_of` should return `true` if the second hash (target)
		/// is a descendent of the first hash (base).
		pub fn find_node_where_mut<F, P, E>(
			&mut self,
			hash: &H,
			number: &N,
			is_descendent_of: &F,
			predicate: &P,
		) -> Result<FindOutcome<&mut Node<H, N, V>>, Error<E>>
			where E: std::error::Error,
				  F: Fn(&H, &H) -> Result<bool, E>,
				  P: Fn(&V) -> bool,
		{
			let outcome = self.find_node_index_where(hash, number, is_descendent_of, predicate)?;

			match outcome {
				FindOutcome::Abort => Ok(FindOutcome::Abort),
				FindOutcome::Failure(f) => Ok(FindOutcome::Failure(f)),
				FindOutcome::Found(mut indexes) => {
					let mut cur = self;

					while let Some(i) = indexes.pop() {
						cur = &mut cur.children[i];
					}
					Ok(FindOutcome::Found(cur))
				},
			}
		}
	}
}

// Workaround for: https://github.com/rust-lang/rust/issues/34537
use node_implementation::{Node, FindOutcome};

struct ForkTreeIterator<'a, H, N, V> {
	stack: Vec<&'a Node<H, N, V>>,
}

impl<'a, H, N, V> Iterator for ForkTreeIterator<'a, H, N, V> {
	type Item = &'a Node<H, N, V>;

	fn next(&mut self) -> Option<Self::Item> {
		self.stack.pop().map(|node| {
			// child nodes are stored ordered by max branch height (decreasing),
			// we want to keep this ordering while iterating but since we're
			// using a stack for iterator state we need to reverse it.
			self.stack.extend(node.children.iter().rev());
			node
		})
	}
}

struct RemovedIterator<H, N, V> {
	stack: Vec<Node<H, N, V>>,
}

impl<H, N, V> Iterator for RemovedIterator<H, N, V> {
	type Item = (H, N, V);

	fn next(&mut self) -> Option<Self::Item> {
		self.stack.pop().map(|mut node| {
			// child nodes are stored ordered by max branch height (decreasing),
			// we want to keep this ordering while iterating but since we're
			// using a stack for iterator state we need to reverse it.
			let mut children = Vec::new();
			std::mem::swap(&mut children, &mut node.children);

			self.stack.extend(children.into_iter().rev());
			(node.hash, node.number, node.data)
		})
	}
}

#[cfg(test)]
mod test {
	use super::{FinalizationResult, ForkTree, Error};
	use codec::{Encode, Decode};

	#[derive(Debug, PartialEq)]
	struct TestError;

	impl std::fmt::Display for TestError {
		fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
			write!(f, "TestError")
		}
	}

	impl std::error::Error for TestError {}

	fn test_fork_tree<'a>() -> (ForkTree<&'a str, u64, u64>, impl Fn(&&str, &&str) -> Result<bool, TestError>)  {
		let mut tree = ForkTree::new();

		//
		//     - B - C - D - E
		//    /
		//   /   - G
		//  /   /
		// A - F - H - I
		//          \
		//           - L - M
		//              \
		//               - O
		//  \
		//   — J - K
		//
		// (where N is not a part of fork tree)
		let is_descendent_of = |base: &&str, block: &&str| -> Result<bool, TestError> {
			let letters = vec!["B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "O"];
			match (*base, *block) {
				("A", b) => Ok(letters.into_iter().any(|n| n == b)),
				("B", b) => Ok(b == "C" || b == "D" || b == "E"),
				("C", b) => Ok(b == "D" || b == "E"),
				("D", b) => Ok(b == "E"),
				("E", _) => Ok(false),
				("F", b) => Ok(b == "G" || b == "H" || b == "I" || b == "L" || b == "M" || b == "O"),
				("G", _) => Ok(false),
				("H", b) => Ok(b == "I" || b == "L" || b == "M" || b == "O"),
				("I", _) => Ok(false),
				("J", b) => Ok(b == "K"),
				("K", _) => Ok(false),
				("L", b) => Ok(b == "M" || b == "O"),
				("M", _) => Ok(false),
				("O", _) => Ok(false),
				("0", _) => Ok(true),
				_ => Ok(false),
			}
		};

		tree.import("A", 1, 10, &is_descendent_of).unwrap();

		tree.import("B", 2, 9, &is_descendent_of).unwrap();
		tree.import("C", 3, 8, &is_descendent_of).unwrap();
		tree.import("D", 4, 7, &is_descendent_of).unwrap();
		tree.import("E", 5, 6, &is_descendent_of).unwrap();

		tree.import("F", 2, 5, &is_descendent_of).unwrap();
		tree.import("G", 3, 4, &is_descendent_of).unwrap();

		tree.import("H", 3, 3, &is_descendent_of).unwrap();
		tree.import("I", 4, 2, &is_descendent_of).unwrap();
		tree.import("L", 4, 1, &is_descendent_of).unwrap();
		tree.import("M", 5, 2, &is_descendent_of).unwrap();
		tree.import("O", 5, 3, &is_descendent_of).unwrap();

		tree.import("J", 2, 4, &is_descendent_of).unwrap();
		tree.import("K", 3, 11, &is_descendent_of).unwrap();

		(tree, is_descendent_of)
	}

	#[test]
	fn find_node_index_where() {
		let (tree, is_descendent_of) = test_fork_tree();

		assert_eq!(
			tree.find_node_index_where(&"B", &2, &is_descendent_of, &|_| true),
			Ok(Some(vec![0]))
		);

		assert_eq!(
			tree.find_node_index_where(&"C", &3, &is_descendent_of, &|_| true),
			Ok(Some(vec![0, 0]))
		);

		assert_eq!(
			tree.find_node_index_where(&"D", &4, &is_descendent_of, &|_| true),
			Ok(Some(vec![0, 0, 0]))
		);

		assert_eq!(
			tree.find_node_index_where(&"E", &5, &is_descendent_of, &|_| true),
			Ok(Some(vec![0, 0, 0, 0]))
		);

		assert_eq!(
			tree.find_node_index_where(&"F", &2, &is_descendent_of, &|_| true),
			Ok(Some(vec![0]))
		);

		assert_eq!(
			tree.find_node_index_where(&"G", &3, &is_descendent_of, &|_| true),
			Ok(Some(vec![1, 0]))
		);

		assert_eq!(
			tree.find_node_index_where(&"H", &3, &is_descendent_of, &|_| true),
			Ok(Some(vec![1, 0]))
		);

		assert_eq!(
			tree.find_node_index_where(&"I", &4, &is_descendent_of, &|_| true),
			Ok(Some(vec![0, 1, 0]))
		);

		assert_eq!(
			tree.find_node_index_where(&"L", &4, &is_descendent_of, &|_| true),
			Ok(Some(vec![0, 1, 0]))
		);

		assert_eq!(
			tree.find_node_index_where(&"M", &5, &is_descendent_of, &|_| true),
			Ok(Some(vec![0, 0, 1, 0]))
		);

		assert_eq!(
			tree.find_node_index_where(&"O", &5, &is_descendent_of, &|_| true),
			Ok(Some(vec![0, 0, 1, 0]))
		);

		assert_eq!(
			tree.find_node_index_where(&"J", &2, &is_descendent_of, &|_| true),
			Ok(Some(vec![0]))
		);

		assert_eq!(
			tree.find_node_index_where(&"K", &3, &is_descendent_of, &|_| true),
			Ok(Some(vec![2, 0]))
		);

		for i in 0 .. 10 {
			assert_eq!(
				tree.find_node_index_where(&"A", &i, &is_descendent_of, &|_| true),
				Ok(None),
				"{}", i
			);
		}

		assert_eq!(
			tree.find_node_index_where(&"B", &0, &is_descendent_of, &|_| true),
			Ok(None),
		);
	}

	#[test]
	fn import_doesnt_revert() {
		let (mut tree, is_descendent_of) = test_fork_tree();

		tree.finalize_root(&"A");

		assert_eq!(
			tree.best_finalized_number,
			Some(1),
		);

		assert_eq!(
			tree.import("A", 1, 1, &is_descendent_of),
			Err(Error::Revert),
		);
	}

	#[test]
	fn import_doesnt_add_duplicates() {
		let (mut tree, is_descendent_of) = test_fork_tree();

		assert_eq!(
			tree.import("A", 1, 1, &is_descendent_of),
			Err(Error::Duplicate),
		);

		assert_eq!(
			tree.import("I", 4, 1, &is_descendent_of),
			Err(Error::Duplicate),
		);

		assert_eq!(
			tree.import("G", 3, 1, &is_descendent_of),
			Err(Error::Duplicate),
		);

		assert_eq!(
			tree.import("K", 3, 1, &is_descendent_of),
			Err(Error::Duplicate),
		);
	}

	#[test]
	fn finalize_root_works() {
		let finalize_a = || {
			let (mut tree, ..) = test_fork_tree();

			assert_eq!(
				tree.roots().map(|(h, n, _)| (h.clone(), n.clone())).collect::<Vec<_>>(),
				vec![("A", 1)],
			);

			// finalizing "A" opens up three possible forks
			tree.finalize_root(&"A");

			assert_eq!(
				tree.roots().map(|(h, n, _)| (h.clone(), n.clone())).collect::<Vec<_>>(),
				vec![("B", 2), ("F", 2), ("J", 2)],
			);

			tree
		};

		{
			let mut tree = finalize_a();

			// finalizing "B" will progress on its fork and remove any other competing forks
			tree.finalize_root(&"B");

			assert_eq!(
				tree.roots().map(|(h, n, _)| (h.clone(), n.clone())).collect::<Vec<_>>(),
				vec![("C", 3)],
			);

			// all the other forks have been pruned
			assert!(tree.roots.len() == 1);
		}

		{
			let mut tree = finalize_a();

			// finalizing "J" will progress on its fork and remove any other competing forks
			tree.finalize_root(&"J");

			assert_eq!(
				tree.roots().map(|(h, n, _)| (h.clone(), n.clone())).collect::<Vec<_>>(),
				vec![("K", 3)],
			);

			// all the other forks have been pruned
			assert!(tree.roots.len() == 1);
		}
	}

	#[test]
	fn finalize_works() {
		let (mut tree, is_descendent_of) = test_fork_tree();

		let original_roots = tree.roots.clone();

		// finalizing a block prior to any in the node doesn't change the tree
		assert_eq!(
			tree.finalize(&"0", 0, &is_descendent_of),
			Ok(FinalizationResult::Unchanged),
		);

		assert_eq!(tree.roots, original_roots);

		// finalizing "A" opens up three possible forks
		assert_eq!(
			tree.finalize(&"A", 1, &is_descendent_of),
			Ok(FinalizationResult::Changed(Some(10))),
		);

		assert_eq!(
			tree.roots().map(|(h, n, _)| (h.clone(), n.clone())).collect::<Vec<_>>(),
			vec![("B", 2), ("F", 2), ("J", 2)],
		);

		// finalizing anything lower than what we observed will fail
		assert_eq!(
			tree.best_finalized_number,
			Some(1),
		);

		assert_eq!(
			tree.finalize(&"Z", 1, &is_descendent_of),
			Err(Error::Revert),
		);

		// trying to finalize a node without finalizing its ancestors first will fail
		assert_eq!(
			tree.finalize(&"H", 3, &is_descendent_of),
			Err(Error::UnfinalizedAncestor),
		);

		// after finalizing "F" we can finalize "H"
		assert_eq!(
			tree.finalize(&"F", 2, &is_descendent_of),
			Ok(FinalizationResult::Changed(Some(5))),
		);

		assert_eq!(
			tree.finalize(&"H", 3, &is_descendent_of),
			Ok(FinalizationResult::Changed(Some(3))),
		);

		assert_eq!(
			tree.roots().map(|(h, n, _)| (h.clone(), n.clone())).collect::<Vec<_>>(),
			vec![("L", 4), ("I", 4)],
		);

		// finalizing a node from another fork that isn't part of the tree clears the tree
		assert_eq!(
			tree.finalize(&"Z", 5, &is_descendent_of),
			Ok(FinalizationResult::Changed(None)),
		);

		assert!(tree.roots.is_empty());
	}

	#[test]
	fn finalize_with_ancestor_works() {
		let (mut tree, is_descendent_of) = test_fork_tree();

		let original_roots = tree.roots.clone();

		// finalizing a block prior to any in the node doesn't change the tree
		assert_eq!(
			tree.finalize_with_ancestors(&"0", 0, &is_descendent_of),
			Ok(FinalizationResult::Unchanged),
		);

		assert_eq!(tree.roots, original_roots);

		// finalizing "A" opens up three possible forks
		assert_eq!(
			tree.finalize_with_ancestors(&"A", 1, &is_descendent_of),
			Ok(FinalizationResult::Changed(Some(10))),
		);

		assert_eq!(
			tree.roots().map(|(h, n, _)| (h.clone(), n.clone())).collect::<Vec<_>>(),
			vec![("B", 2), ("F", 2), ("J", 2)],
		);

		// finalizing H:
		// 1) removes roots that are not ancestors/descendants of H (B, J)
		// 2) opens root that is ancestor of H (F -> G+H)
		// 3) finalizes the just opened root H (H -> I + L)
		assert_eq!(
			tree.finalize_with_ancestors(&"H", 3, &is_descendent_of),
			Ok(FinalizationResult::Changed(Some(3))),
		);

		assert_eq!(
			tree.roots().map(|(h, n, _)| (h.clone(), n.clone())).collect::<Vec<_>>(),
			vec![("L", 4), ("I", 4)],
		);

		assert_eq!(
			tree.best_finalized_number,
			Some(3),
		);

		// finalizing N (which is not a part of the tree):
		// 1) removes roots that are not ancestors/descendants of N (I)
		// 2) opens root that is ancestor of N (L -> M+O)
		// 3) removes roots that are not ancestors/descendants of N (O)
		// 4) opens root that is ancestor of N (M -> {})
		assert_eq!(
			tree.finalize_with_ancestors(&"N", 6, &is_descendent_of),
			Ok(FinalizationResult::Changed(None)),
		);

		assert_eq!(
			tree.roots().map(|(h, n, _)| (h.clone(), n.clone())).collect::<Vec<_>>(),
			vec![],
		);

		assert_eq!(
			tree.best_finalized_number,
			Some(6),
		);
	}

	#[test]
	fn finalize_with_descendent_works() {
		#[derive(Debug, PartialEq)]
		struct Change { effective: u64 };

		let (mut tree, is_descendent_of) = {
			let mut tree = ForkTree::new();

			let is_descendent_of = |base: &&str, block: &&str| -> Result<bool, TestError> {

				//
				// A0 #1 - (B #2) - (C #5) - D #10 - E #15 - (F #100)
				//                            \
				//                             - (G #100)
				//
				// A1 #1
				//
				// Nodes B, C, F and G  are not part of the tree.
				match (*base, *block) {
					("A0", b) => Ok(b == "B" || b == "C" || b == "D" || b == "G"),
					("A1", _) => Ok(false),
					("C", b) => Ok(b == "D"),
					("D", b) => Ok(b == "E" || b == "F" || b == "G"),
					("E", b) => Ok(b == "F"),
					_ => Ok(false),
				}
			};

			tree.import("A0", 1, Change { effective: 5 }, &is_descendent_of).unwrap();
			tree.import("A1", 1, Change { effective: 5 }, &is_descendent_of).unwrap();
			tree.import("D", 10, Change { effective: 10 }, &is_descendent_of).unwrap();
			tree.import("E", 15, Change { effective: 50 }, &is_descendent_of).unwrap();

			(tree, is_descendent_of)
		};

		assert_eq!(
			tree.finalizes_any_with_descendent_if(
				&"B",
				2,
				&is_descendent_of,
				|c| c.effective <= 2,
			),
			Ok(None),
		);

		// finalizing "D" will finalize a block from the tree, but it can't be applied yet
		// since it is not a root change
		assert_eq!(
			tree.finalizes_any_with_descendent_if(
				&"D",
				10,
				&is_descendent_of,
				|c| c.effective == 10,
			),
			Ok(Some(false)),
		);

		// finalizing "B" doesn't finalize "A0" since the predicate doesn't pass,
		// although it will clear out "A1" from the tree
		assert_eq!(
			tree.finalize_with_descendent_if(
				&"B",
				2,
				&is_descendent_of,
				|c| c.effective <= 2,
			),
			Ok(FinalizationResult::Changed(None)),
		);

		assert_eq!(
			tree.roots().map(|(h, n, _)| (h.clone(), n.clone())).collect::<Vec<_>>(),
			vec![("A0", 1)],
		);

		// finalizing "C" will finalize the node "A0" and prune it out of the tree
		assert_eq!(
			tree.finalizes_any_with_descendent_if(
				&"C",
				5,
				&is_descendent_of,
				|c| c.effective <= 5,
			),
			Ok(Some(true)),
		);

		assert_eq!(
			tree.finalize_with_descendent_if(
				&"C",
				5,
				&is_descendent_of,
				|c| c.effective <= 5,
			),
			Ok(FinalizationResult::Changed(Some(Change { effective: 5 }))),
		);

		assert_eq!(
			tree.roots().map(|(h, n, _)| (h.clone(), n.clone())).collect::<Vec<_>>(),
			vec![("D", 10)],
		);

		// finalizing "F" will fail since it would finalize past "E" without finalizing "D" first
		assert_eq!(
			tree.finalizes_any_with_descendent_if(
				&"F",
				100,
				&is_descendent_of,
				|c| c.effective <= 100,
			),
			Err(Error::UnfinalizedAncestor),
		);

		// it will work with "G" though since it is not in the same branch as "E"
		assert_eq!(
			tree.finalizes_any_with_descendent_if(
				&"G",
				100,
				&is_descendent_of,
				|c| c.effective <= 100,
			),
			Ok(Some(true)),
		);

		assert_eq!(
			tree.finalize_with_descendent_if(
				&"G",
				100,
				&is_descendent_of,
				|c| c.effective <= 100,
			),
			Ok(FinalizationResult::Changed(Some(Change { effective: 10 }))),
		);

		// "E" will be pruned out
		assert_eq!(tree.roots().count(), 0);
	}

	#[test]
	fn iter_iterates_in_preorder() {
		let (tree, ..) = test_fork_tree();
		assert_eq!(
			tree.iter().map(|(h, n, _)| (h.clone(), n.clone())).collect::<Vec<_>>(),
			vec![
				("A", 1),
					("B", 2),
						("C", 3),
							("D", 4),
								("E", 5),
					("F", 2),
						("H", 3),
							("L", 4),
								("M", 5),
								("O", 5),
							("I", 4),
						("G", 3),
					("J", 2),
						("K", 3),
			],
		);
	}

	#[test]
	fn minimizes_calls_to_is_descendent_of() {
		use std::sync::atomic::{AtomicUsize, Ordering};

		let n_is_descendent_of_calls = AtomicUsize::new(0);

		let is_descendent_of = |_: &&str, _: &&str| -> Result<bool, TestError> {
			n_is_descendent_of_calls.fetch_add(1, Ordering::SeqCst);
			Ok(true)
		};

		{
			// Deep tree where we want to call `finalizes_any_with_descendent_if`. The
			// search for the node should first check the predicate (which is cheaper) and
			// only then call `is_descendent_of`
			let mut tree = ForkTree::new();
			let letters = vec!["A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K"];

			for (i, letter) in letters.iter().enumerate() {
				tree.import::<_, TestError>(*letter, i, i, &|_, _| Ok(true)).unwrap();
			}

			// "L" is a descendent of "K", but the predicate will only pass for "K",
			// therefore only one call to `is_descendent_of` should be made
			assert_eq!(
				tree.finalizes_any_with_descendent_if(
					&"L",
					11,
					&is_descendent_of,
					|i| *i == 10,
				),
				Ok(Some(false)),
			);

			assert_eq!(
				n_is_descendent_of_calls.load(Ordering::SeqCst),
				1,
			);
		}

		n_is_descendent_of_calls.store(0, Ordering::SeqCst);

		{
			// Multiple roots in the tree where we want to call `finalize_with_descendent_if`.
			// The search for the root node should first check the predicate (which is cheaper)
			// and only then call `is_descendent_of`
			let mut tree = ForkTree::new();
			let letters = vec!["A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K"];

			for (i, letter) in letters.iter().enumerate() {
				tree.import::<_, TestError>(*letter, i, i, &|_, _| Ok(false)).unwrap();
			}

			// "L" is a descendent of "K", but the predicate will only pass for "K",
			// therefore only one call to `is_descendent_of` should be made
			assert_eq!(
				tree.finalize_with_descendent_if(
					&"L",
					11,
					&is_descendent_of,
					|i| *i == 10,
				),
				Ok(FinalizationResult::Changed(Some(10))),
			);

			assert_eq!(
				n_is_descendent_of_calls.load(Ordering::SeqCst),
				1,
			);
		}
	}

	#[test]
	fn find_node_works() {
		let (tree, is_descendent_of) = test_fork_tree();

		let node = tree.find_node_where(
			&"D",
			&4,
			&is_descendent_of,
			&|_| true,
		).unwrap().unwrap();

		assert_eq!(node.hash, "C");
		assert_eq!(node.number, 3);
	}

	#[test]
	fn map_works() {
		let (tree, _is_descendent_of) = test_fork_tree();

		let _tree = tree.map(&mut |_, _, _| ());
	}

	#[test]
	fn prune_works() {
		let (mut tree, is_descendent_of) = test_fork_tree();

		let removed = tree.prune(
			&"C",
			&3,
			&is_descendent_of,
			&|_| true,
		).unwrap();

		assert_eq!(
			tree.roots.iter().map(|node| node.hash).collect::<Vec<_>>(),
			vec!["B"],
		);

		assert_eq!(
			tree.iter().map(|(hash, _, _)| *hash).collect::<Vec<_>>(),
			vec!["B", "C", "D", "E"],
		);

		assert_eq!(
			removed.map(|(hash, _, _)| hash).collect::<Vec<_>>(),
			vec!["A", "F", "H", "L", "M", "O", "I", "G", "J", "K"]
		);

		let removed = tree.prune(
			&"E",
			&5,
			&is_descendent_of,
			&|_| true,
		).unwrap();

		assert_eq!(
			tree.roots.iter().map(|node| node.hash).collect::<Vec<_>>(),
			vec!["D"],
		);

		assert_eq!(
			tree.iter().map(|(hash, _, _)| *hash).collect::<Vec<_>>(),
			vec!["D", "E"],
		);

		assert_eq!(
			removed.map(|(hash, _, _)| hash).collect::<Vec<_>>(),
			vec!["B", "C"]
		);
	}

	#[test]
	fn find_node_backtracks_after_finding_highest_descending_node() {
		let mut tree = ForkTree::new();

		//
		// A - B
		//  \
		//   — C
		//
		let is_descendent_of = |base: &&str, block: &&str| -> Result<bool, TestError> {
			match (*base, *block) {
				("A", b) => Ok(b == "B" || b == "C" || b == "D"),
				("B", b) | ("C", b) => Ok(b == "D"),
				("0", _) => Ok(true),
				_ => Ok(false),
			}
		};

		tree.import("A", 1, 1, &is_descendent_of).unwrap();
		tree.import("B", 2, 2, &is_descendent_of).unwrap();
		tree.import("C", 2, 4, &is_descendent_of).unwrap();

		// when searching the tree we reach node `C`, but the
		// predicate doesn't pass. we should backtrack to `B`, but not to `A`,
		// since "B" fulfills the predicate.
		let node = tree.find_node_where(
			&"D",
			&3,
			&is_descendent_of,
			&|data| *data < 3,
		).unwrap();

		assert_eq!(node.unwrap().hash, "B");
	}

	#[test]
	fn tree_rebalance() {
		let (mut tree, _) = test_fork_tree();

		assert_eq!(
			tree.iter().map(|(h, _, _)| *h).collect::<Vec<_>>(),
			vec!["A", "B", "C", "D", "E", "F", "H", "L", "M", "O", "I", "G", "J", "K"],
		);

		// after rebalancing the tree we should iterate in preorder exploring
		// the longest forks first. check the ascii art above to understand the
		// expected output below.
		tree.rebalance();

		assert_eq!(
			tree.iter().map(|(h, _, _)| *h).collect::<Vec<_>>(),
			["A", "B", "C", "D", "E", "F", "H", "L", "M", "O", "I", "G", "J", "K"]
		);
	}

	#[test]
	fn find_node_where_value() {
		let (tree, d) = test_fork_tree();
		assert_eq!(
			tree.find_node_where(&"M", &5, &d, &|&n| n == 1 || n == 2)
				.map(|opt| opt.map(|node| node.hash)),
			Ok(Some("L")),
			"{:?}", tree.find_node_index_where(&"M", &5, &d, &|&n| n == 1 || n == 2)
		);
	}

	#[test]
	fn find_node_where_value_2() {
		let mut tree = ForkTree::new();

		//
		// A - B
		//  \
		//   — C
		//
		let is_descendent_of = |base: &&str, block: &&str| -> Result<bool, TestError> {
			match (*base, *block) {
				("A", b) => Ok(b == "B" || b == "C" || b == "D"),
				("B", b) | ("C", b) => Ok(b == "D"),
				("0", _) => Ok(true),
				_ => Ok(false),
			}
		};

		tree.import("A", 1, 1, &is_descendent_of).unwrap();
		tree.import("B", 2, 2, &is_descendent_of).unwrap();
		tree.import("C", 2, 4, &is_descendent_of).unwrap();

		assert_eq!(
			tree.find_node_where(&"D", &3, &is_descendent_of, &|&n| n == 1)
				.map(|opt| opt.map(|node| node.hash)),
			Ok(Some("A"))
		);
	}

	#[test]
	fn encoding_and_decoding_works() {
		let	tree = {
			let mut tree = ForkTree::<String, u64, u64>::new();
	
			//
			//     - B - C - D - E
			//    /
			//   /   - G
			//  /   /
			// A - F - H - I
			//          \
			//           - L - M
			//              \
			//               - O
			//  \
			//   — J - K
			//
			// (where N is not a part of fork tree)
			let is_descendent_of = |base: &String, block: &String| -> Result<bool, TestError> {
				let letters = vec!["B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "O"];
				match (&base[..], &block[..]) {
					("A", b) => Ok(letters.into_iter().any(|n| n == b)),
					("B", b) => Ok(b == "C" || b == "D" || b == "E"),
					("C", b) => Ok(b == "D" || b == "E"),
					("D", b) => Ok(b == "E"),
					("E", _) => Ok(false),
					("F", b) => Ok(b == "G" || b == "H" || b == "I" || b == "L" || b == "M" || b == "O"),
					("G", _) => Ok(false),
					("H", b) => Ok(b == "I" || b == "L" || b == "M" || b == "O"),
					("I", _) => Ok(false),
					("J", b) => Ok(b == "K"),
					("K", _) => Ok(false),
					("L", b) => Ok(b == "M" || b == "O"),
					("M", _) => Ok(false),
					("O", _) => Ok(false),
					("0", _) => Ok(true),
					_ => Ok(false),
				}
			};
	
			tree.import("A".into(), 1, 10, &is_descendent_of).unwrap();
	
			tree.import("B".into(), 2, 9, &is_descendent_of).unwrap();
			tree.import("C".into(), 3, 8, &is_descendent_of).unwrap();
			tree.import("D".into(), 4, 7, &is_descendent_of).unwrap();
			tree.import("E".into(), 5, 6, &is_descendent_of).unwrap();
	
			tree.import("F".into(), 2, 5, &is_descendent_of).unwrap();
			tree.import("G".into(), 3, 4, &is_descendent_of).unwrap();
	
			tree.import("H".into(), 3, 3, &is_descendent_of).unwrap();
			tree.import("I".into(), 4, 2, &is_descendent_of).unwrap();
			tree.import("L".into(), 4, 1, &is_descendent_of).unwrap();
			tree.import("M".into(), 5, 2, &is_descendent_of).unwrap();
			tree.import("O".into(), 5, 3, &is_descendent_of).unwrap();
	
			tree.import("J".into(), 2, 4, &is_descendent_of).unwrap();
			tree.import("K".into(), 3, 11, &is_descendent_of).unwrap();
	
			tree
		};

		let encoded = tree.encode();
		let decoded = ForkTree::decode(&mut &encoded[..]).unwrap();
		assert_eq!(tree, decoded);
	}
}
