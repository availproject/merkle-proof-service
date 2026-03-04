use sha2::{Digest, Sha256};

/// Compute the Merkle tree branch (proof) for the given index.
///
/// The `data_roots` slice must have exactly `commitment_tree_size` elements.
/// Returns the sibling hashes needed to reconstruct the root from the leaf at `index`.
pub fn compute_merkle_branch(
    commitment_tree_size: usize,
    data_roots: &[[u8; 32]],
    index: usize,
) -> Result<Vec<[u8; 32]>, &'static str> {
    if data_roots.len() != commitment_tree_size {
        return Err("Invalid number of leaves");
    }

    let mut nodes: Vec<[u8; 32]> = data_roots.to_vec();
    let mut branch: Vec<[u8; 32]> = Vec::new();
    let mut index_so_far = index;

    while nodes.len() > 1 {
        let mut next_level: Vec<[u8; 32]> = Vec::new();

        for i in (0..nodes.len()).step_by(2) {
            let left = &nodes[i];
            let right = &nodes[i + 1];

            let mut hasher = Sha256::new();
            hasher.update(left);
            hasher.update(right);
            let hash: [u8; 32] = hasher.finalize().into();
            next_level.push(hash);

            let aligned_index = index_so_far - (index_so_far % 2);
            if aligned_index == i {
                if index_so_far % 2 == 0 {
                    branch.push(*right);
                } else {
                    branch.push(*left);
                }
            }
        }

        index_so_far /= 2;
        nodes = next_level;
    }

    Ok(branch)
}

/// Verify that a Merkle branch is valid against the expected root (data commitment).
pub fn verify_merkle_branch(
    leaf: &[u8; 32],
    branch: &[[u8; 32]],
    index: usize,
    expected_root: &[u8; 32],
) -> bool {
    let mut current_hash = *leaf;
    let mut index_so_far = index;

    for sibling in branch {
        let mut hasher = Sha256::new();
        if index_so_far % 2 == 0 {
            hasher.update(current_hash);
            hasher.update(sibling);
        } else {
            hasher.update(sibling);
            hasher.update(current_hash);
        }
        current_hash = hasher.finalize().into();
        index_so_far /= 2;
    }

    current_hash == *expected_root
}

/// Compute the Merkle root from a set of data roots.
#[allow(dead_code)]
pub fn compute_data_commitment(
    data_roots: &[[u8; 32]],
    commitment_tree_size: usize,
) -> Result<[u8; 32], &'static str> {
    if data_roots.len() != commitment_tree_size {
        return Err("Data roots length must match commitment tree size");
    }

    let mut level: Vec<[u8; 32]> = data_roots.to_vec();

    while level.len() > 1 {
        let mut next_level: Vec<[u8; 32]> = Vec::new();

        for i in (0..level.len()).step_by(2) {
            let mut hasher = Sha256::new();
            hasher.update(level[i]);
            hasher.update(level[i + 1]);
            let hash: [u8; 32] = hasher.finalize().into();
            next_level.push(hash);
        }

        level = next_level;
    }

    Ok(level[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merkle_branch_and_verification() {
        let leaves: Vec<[u8; 32]> = (0..4u8)
            .map(|i| {
                let mut leaf = [0u8; 32];
                leaf[0] = i;
                leaf
            })
            .collect();

        let branch = compute_merkle_branch(4, &leaves, 1).unwrap();
        let root = compute_data_commitment(&leaves, 4).unwrap();
        assert!(verify_merkle_branch(&leaves[1], &branch, 1, &root));
    }

    #[test]
    fn test_invalid_leaf_count() {
        let leaves = vec![[0u8; 32]; 3];
        let result = compute_merkle_branch(4, &leaves, 0);
        assert!(result.is_err());
    }
}
