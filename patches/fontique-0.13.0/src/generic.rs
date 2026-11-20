// Copyright 2024 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Generic font families.

use super::FamilyId;
use crate::GenericFamily;
use smallvec::SmallVec;

type FamilyVec = SmallVec<[FamilyId; 2]>;
const COUNT: usize = GenericFamily::MAX_VALUE as usize + 1;

/// Maps generic families to family identifiers.
#[derive(Clone, Default, Debug)]
pub struct GenericFamilyMap {
    map: [FamilyVec; COUNT],
    /// known-future (outside round 5): generic families whose own list replaces the system's (one bit each)
    exclusive: u32,
}

impl GenericFamilyMap {
    /// Returns the associated family identifiers for the given generic family.
    pub fn get(&self, generic: GenericFamily) -> &[FamilyId] {
        &self.map[generic as usize]
    }

    /// Sets the associated family identifiers for the given generic family.
    pub fn set(&mut self, generic: GenericFamily, families: impl Iterator<Item = FamilyId>) {
        let map = &mut self.map[generic as usize];
        map.clear();
        map.extend(families);
    }

    /// known-future (outside round 5): whether any generic family's list replaces the system's
    pub fn any_exclusive(&self) -> bool {
        self.exclusive != 0
    }

    /// known-future (outside round 5): whether the given generic family's list replaces the system's
    pub fn is_exclusive(&self, generic: GenericFamily) -> bool {
        self.exclusive & (1 << generic as u32) != 0
    }

    /// known-future (outside round 5): sets the generic family's list and makes it replace the system's
    pub fn set_exclusive(&mut self, generic: GenericFamily, families: impl Iterator<Item = FamilyId>) {
        self.set(generic, families);
        self.exclusive |= 1 << generic as u32;
    }

    /// Appends the family identifiers to the list for the given generic family.
    pub fn append(&mut self, generic: GenericFamily, families: impl Iterator<Item = FamilyId>) {
        self.map[generic as usize].extend(families);
    }
}
