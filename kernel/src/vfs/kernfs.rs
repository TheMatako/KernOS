// kernel/src/vfs/kernfs.rs
//
// KernFS — filesystem RAM haute performance — Brick 8, part 3/3.
//
// ── Pourquoi KernFS plutôt qu'ext2 ? ─────────────────────────────────────────
//
// ext2 a été conçu pour des disques rotatifs des années 1990 :
//   - Blocs de 1 KiB avec listes chaînées d'indirections → cache-unfriendly.
//   - Métadonnées éparpillées sur le "disque" → plusieurs lectures pour stat().
//   - Journalisation inutile en RAM (si ça crash, tout est perdu quand même).
//
// KernFS est conçu pour un RAM disk :
//   - Blocs de 4 KiB alignés sur les frames PMM → zéro padding.
//   - Table d'inodes plate en RAM → lookup O(1) sans aller-retour "disque".
//   - Extents contigus (plages de blocs) → read/write = un seul memcpy.
//   - Pas de journalisation : volatile by design.
//   - Bitmap de blocs en mémoire : alloc/free O(N/64) avec scan 64 bits.
//
// ── Layout sur le RAM disk ────────────────────────────────────────────────────
//
//   Bloc 0          : Superbloc (512 premiers octets du bloc)
//   Blocs 1–N       : Table d'inodes (MAX_INODES × sizeof(KernInode))
//   Blocs N+1–M     : Bitmap de blocs (1 bit par bloc de données)
//   Blocs M+1–fin   : Données (fichiers + entrées de répertoires)
//
// Toutes les structures sont en mémoire (dans KERNFS_STATE statique) ;
// le RAM disk est utilisé comme backing store pour persister à travers les
// reboots… ce qui n'a aucun sens pour un RAM disk, mais facilite le test
// avec un dump mémoire.  En pratique tout est accédé via des pointeurs
// directs dans la carte physique (vmm::phys_to_virt).
//
// ── Extents ──────────────────────────────────────────────────────────────────
//
// Un extent = (bloc_debut, nb_blocs_contigus).
// Chaque inode peut avoir jusqu'à MAX_EXTENTS extents.
// Pour les petits fichiers (< 1 MiB), un seul extent suffit.
// Pour les fichiers fragmentés (après de nombreuses suppressions), plusieurs
// extents sont chaînés.  La fragmentation est absente en pratique sur un
// RAM disk frais.
//
// ── Répertoires ───────────────────────────────────────────────────────────────
//
// Un répertoire est un fichier dont les données sont un tableau de
// `RawDirEntry` (32 octets chacune).  La recherche est linéaire O(N) sur le
// nombre d'entrées — acceptable pour des répertoires < 1 000 entrées.

#![allow(dead_code)]
#![allow(static_mut_refs)]

use super::{DirEntry, Filesystem, InodeKind, InodeMeta, NAME_MAX};
use crate::drivers::block;

// ---------------------------------------------------------------------------
// Constantes KernFS
// ---------------------------------------------------------------------------

/// Taille d'un bloc KernFS — identique à la taille d'un frame PMM.
/// Un bloc = un secteur × 8 (512 × 8 = 4096).
pub const BLOCK_SIZE: usize = 4096;

/// Nombre de secteurs RAM disk par bloc KernFS.
pub const SECTORS_PER_BLOCK: usize = BLOCK_SIZE / block::SECTOR_SIZE; // 8

/// Nombre maximum d'inodes dans le filesystem.
/// 4096 inodes = 4096 fichiers/répertoires maximum.
pub const MAX_INODES: usize = 4096;

/// Nombre maximum d'extents par inode.
/// Avec des extents de ~512 blocs en moyenne, 8 extents = ~16 MiB par fichier.
pub const MAX_EXTENTS: usize = 8;

/// Nombre maximum de blocs de données (hors superbloc, inodes, bitmap).
/// 8192 blocs × 4 KiB = 32 MiB → adapté au RAM disk de 32 MiB.
pub const MAX_DATA_BLOCKS: usize = 8192;

/// Numéro d'inode de la racine ("/").  Conventionnellement 1 (0 = invalide).
pub const ROOT_INODE: u64 = 1;

/// Taille d'une entrée de répertoire sur disque.
const RAW_DIRENT_SIZE: usize = core::mem::size_of::<RawDirEntry>(); // 32 octets

/// Nombre d'entrées de répertoire par bloc.
const DIRENTS_PER_BLOCK: usize = BLOCK_SIZE / RAW_DIRENT_SIZE;

// ---------------------------------------------------------------------------
// Structures on-disk / in-memory
// ---------------------------------------------------------------------------

/// Un extent : plage de blocs de données contigus.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct Extent {
    /// Premier bloc de données de cet extent (index dans le pool de données).
    pub start: u32,
    /// Nombre de blocs contigus dans cet extent.
    pub count: u32,
}

impl Extent {
    fn is_empty(self) -> bool {
        self.count == 0
    }
}

/// Un inode KernFS — stocké dans la table d'inodes en mémoire.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct KernInode {
    /// Numéro d'inode (1-based ; 0 = slot libre).
    pub nr: u64,
    /// Type : fichier, répertoire, lien symbolique.
    pub kind: InodeKind,
    /// Taille du contenu en octets.
    pub size: u64,
    /// Permissions UNIX (rwxrwxrwx).
    pub mode: u16,
    /// Propriétaire.
    pub uid: u32,
    /// Groupe.
    pub gid: u32,
    /// Nombre de liens durs pointant vers cet inode.
    pub nlinks: u32,
    /// Extents de données (jusqu'à MAX_EXTENTS).
    pub extents: [Extent; MAX_EXTENTS],
    /// Nombre d'extents utilisés.
    pub n_extents: u8,
}

impl KernInode {
    const fn empty() -> Self {
        Self {
            nr: 0,
            kind: InodeKind::File,
            size: 0,
            mode: 0o644,
            uid: 0,
            gid: 0,
            nlinks: 0,
            extents: [Extent { start: 0, count: 0 }; MAX_EXTENTS],
            n_extents: 0,
        }
    }

    /// Nombre total de blocs de données alloués à cet inode.
    pub fn total_blocks(&self) -> u32 {
        self.extents[..self.n_extents as usize]
            .iter()
            .map(|e| e.count)
            .sum()
    }
}

/// Entrée de répertoire stockée dans les données d'un inode répertoire.
/// Taille fixe : 32 octets.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct RawDirEntry {
    /// Numéro d'inode (0 = entrée supprimée / libre).
    inode_nr: u64, // 8 octets
    /// Type du nœud pointé.
    kind: InodeKind, // 1 octet
    _pad: [u8; 7], // 7 octets de padding
    /// Nom (16 octets max dans une entrée de répertoire compacte).
    /// Pour les noms > 16 caractères, plusieurs entrées sont chaînées
    /// (non implémenté ici — NAME_MAX est limité à 15 en pratique pour
    /// rester dans 32 octets).
    name: [u8; 16], // 16 octets
} // total : 32 octets

impl RawDirEntry {
    fn is_free(self) -> bool {
        self.inode_nr == 0
    }

    fn name_str(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&self.name[..len]).unwrap_or("<bad>")
    }
}

/// Superbloc KernFS — stocké au début du bloc 0.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct Superblock {
    /// Signature magique pour détecter un filesystem valide.
    magic: u64,
    /// Nombre total de blocs de données.
    total_blocks: u32,
    /// Nombre total d'inodes.
    total_inodes: u32,
    /// Nombre de blocs libres.
    free_blocks: u32,
    /// Nombre d'inodes libres.
    free_inodes: u32,
    /// Taille d'un bloc en octets.
    block_size: u32,
}

/// Valeur magique identifiant un volume KernFS.
const KERNFS_MAGIC: u64 = 0x4B45524E_46530001; // "KERNFS\x00\x01"

// ---------------------------------------------------------------------------
// État global KernFS (en mémoire — rien n'est vraiment "sur disque")
// ---------------------------------------------------------------------------

/// L'état complet du filesystem KernFS, stocké en `.bss`.
struct KernFsState {
    /// Table d'inodes (indexée par inode_nr - 1).
    inodes: [KernInode; MAX_INODES],
    /// Bitmap de blocs de données : bit 0 = bloc 0 libre, bit 1 = utilisé.
    block_bitmap: [u64; MAX_DATA_BLOCKS / 64],
    /// Superbloc en mémoire.
    sb: Superblock,
    /// Initialisé ?
    ready: bool,
}

static mut KERNFS_STATE: KernFsState = KernFsState {
    inodes: [KernInode::empty(); MAX_INODES],
    block_bitmap: [0u64; MAX_DATA_BLOCKS / 64],
    sb: Superblock {
        magic: KERNFS_MAGIC,
        total_blocks: MAX_DATA_BLOCKS as u32,
        total_inodes: MAX_INODES as u32,
        free_blocks: MAX_DATA_BLOCKS as u32,
        free_inodes: (MAX_INODES - 1) as u32, // inode 1 = root
        block_size: BLOCK_SIZE as u32,
    },
    ready: false,
};

// ---------------------------------------------------------------------------
// Bitmap helpers
// ---------------------------------------------------------------------------

/// Alloue `count` blocs de données contigus.
///
/// Retourne l'index du premier bloc, ou `None` si insuffisant.
///
/// # Safety
/// Écrit dans `KERNFS_STATE.block_bitmap`.
unsafe fn alloc_blocks(count: u32) -> Option<u32> {
    let state = &mut KERNFS_STATE;
    let n = count as usize;

    // Chercher une plage de `n` bits consécutifs à 0.
    let mut run_start = 0usize;
    let mut run_len = 0usize;

    for block in 0..MAX_DATA_BLOCKS {
        let word = block / 64;
        let bit = block % 64;
        let used = (state.block_bitmap[word] >> bit) & 1 == 1;

        if !used {
            if run_len == 0 {
                run_start = block;
            }
            run_len += 1;
            if run_len == n {
                // Marquer les blocs comme utilisés.
                for b in run_start..run_start + n {
                    let w = b / 64;
                    let k = b % 64;
                    state.block_bitmap[w] |= 1u64 << k;
                }
                state.sb.free_blocks -= count;
                return Some(run_start as u32);
            }
        } else {
            run_len = 0;
        }
    }
    None
}

/// Libère `count` blocs de données à partir de `start`.
///
/// # Safety
/// Écrit dans `KERNFS_STATE.block_bitmap`.
unsafe fn free_blocks(start: u32, count: u32) {
    let state = &mut KERNFS_STATE;
    for b in start as usize..(start + count) as usize {
        let w = b / 64;
        let k = b % 64;
        state.block_bitmap[w] &= !(1u64 << k);
    }
    state.sb.free_blocks += count;
}

// ---------------------------------------------------------------------------
// Inode helpers
// ---------------------------------------------------------------------------

/// Alloue un nouvel inode et retourne son numéro (1-based).
///
/// # Safety
/// Écrit dans `KERNFS_STATE.inodes`.
unsafe fn alloc_inode(kind: InodeKind, mode: u16) -> Option<u64> {
    let state = &mut KERNFS_STATE;
    // L'inode 0 est réservé (invalide) ; on commence à l'index 1.
    for i in 1..MAX_INODES {
        if state.inodes[i].nr == 0 {
            state.inodes[i] = KernInode {
                nr: i as u64,
                kind,
                size: 0,
                mode,
                uid: 0,
                gid: 0,
                nlinks: 1,
                extents: [Extent { start: 0, count: 0 }; MAX_EXTENTS],
                n_extents: 0,
            };
            state.sb.free_inodes -= 1;
            return Some(i as u64);
        }
    }
    None
}

/// Retourne une référence mutable à l'inode `nr`.
///
/// # Safety
/// Accède à `KERNFS_STATE.inodes`.
unsafe fn get_inode(nr: u64) -> Option<&'static mut KernInode> {
    if nr == 0 || nr as usize >= MAX_INODES {
        return None;
    }
    let inode = &mut KERNFS_STATE.inodes[nr as usize];
    if inode.nr == 0 {
        return None;
    }
    Some(inode)
}

/// Libère un inode et tous ses blocs de données.
///
/// # Safety
/// Écrit dans `KERNFS_STATE`.
unsafe fn free_inode(nr: u64) {
    let state = &mut KERNFS_STATE;
    let idx = nr as usize;
    if idx == 0 || idx >= MAX_INODES {
        return;
    }
    let inode = &mut state.inodes[idx];
    let n = inode.n_extents as usize;
    for i in 0..n {
        free_blocks(inode.extents[i].start, inode.extents[i].count);
    }
    *inode = KernInode::empty();
    state.sb.free_inodes += 1;
}

// ---------------------------------------------------------------------------
// Lecture / écriture de blocs via le RAM disk
// ---------------------------------------------------------------------------

/// Lit `BLOCK_SIZE` octets du bloc de données `block_idx` dans `buf`.
///
/// # Safety
/// Appelle `block::read_sector` → nécessite que le driver block soit init.
unsafe fn read_block(block_idx: u32, buf: &mut [u8; BLOCK_SIZE]) {
    let lba_base = data_block_to_lba(block_idx);
    for s in 0..SECTORS_PER_BLOCK {
        let sector_buf = &mut buf[s * block::SECTOR_SIZE..(s + 1) * block::SECTOR_SIZE];
        block::read_sector(lba_base + s, sector_buf).expect("kernfs: read_block failed");
    }
}

/// Écrit `BLOCK_SIZE` octets dans le bloc de données `block_idx`.
///
/// # Safety
/// Appelle `block::write_sector`.
unsafe fn write_block(block_idx: u32, buf: &[u8; BLOCK_SIZE]) {
    let lba_base = data_block_to_lba(block_idx);
    for s in 0..SECTORS_PER_BLOCK {
        let sector_buf = &buf[s * block::SECTOR_SIZE..(s + 1) * block::SECTOR_SIZE];
        block::write_sector(lba_base + s, sector_buf).expect("kernfs: write_block failed");
    }
}

/// Convertit un index de bloc de données en LBA (Logical Block Address).
///
/// Les blocs de données commencent après le superbloc, la table d'inodes et
/// le bitmap.  Calcul fixe : on réserve les 64 premiers secteurs (32 KiB)
/// pour les métadonnées, ce qui est largement suffisant pour 4096 inodes.
fn data_block_to_lba(block_idx: u32) -> usize {
    // Secteurs réservés aux métadonnées :
    //   - 8 secteurs = 1 bloc = superbloc
    //   - 56 secteurs = 7 blocs = table d'inodes + bitmap
    // Total : 64 secteurs (32 KiB).
    64 + block_idx as usize * SECTORS_PER_BLOCK
}

// ---------------------------------------------------------------------------
// Recherche d'entrée dans un répertoire
// ---------------------------------------------------------------------------

/// Cherche `name` dans les données du répertoire `dir_inode`.
///
/// Retourne `(block_idx, entry_idx_in_block, RawDirEntry)` si trouvé.
///
/// # Safety
/// Lit des blocs via `read_block`.
unsafe fn find_dirent(dir_inode: &KernInode, name: &str) -> Option<(u32, usize, RawDirEntry)> {
    let mut buf = [0u8; BLOCK_SIZE];
    let n_extents = dir_inode.n_extents as usize;

    for ext_i in 0..n_extents {
        let ext = dir_inode.extents[ext_i];
        for b in 0..ext.count {
            let block_idx = ext.start + b;
            read_block(block_idx, &mut buf);

            for e in 0..DIRENTS_PER_BLOCK {
                let offset = e * RAW_DIRENT_SIZE;
                let entry: RawDirEntry =
                    core::ptr::read_unaligned(buf[offset..].as_ptr() as *const RawDirEntry);
                if !entry.is_free() && entry.name_str() == name {
                    return Some((block_idx, e, entry));
                }
            }
        }
    }
    None
}

/// Ajoute une entrée dans le répertoire `dir_inode`.
///
/// Cherche un slot libre dans les blocs existants ; si aucun, alloue un
/// nouveau bloc.
///
/// # Safety
/// Lit/écrit des blocs via `read_block` / `write_block`.
unsafe fn add_dirent(
    dir_inode: &mut KernInode,
    name: &str,
    child_nr: u64,
    kind: InodeKind,
) -> Result<(), &'static str> {
    let mut buf = [0u8; BLOCK_SIZE];
    let name_bytes = name.as_bytes();
    let name_len = name_bytes.len().min(16);

    // Construire l'entrée.
    let mut raw = RawDirEntry {
        inode_nr: child_nr,
        kind,
        _pad: [0u8; 7],
        name: [0u8; 16],
    };
    raw.name[..name_len].copy_from_slice(&name_bytes[..name_len]);

    // Chercher un slot libre dans les blocs existants.
    let n_extents = dir_inode.n_extents as usize;
    for ext_i in 0..n_extents {
        let ext = dir_inode.extents[ext_i];
        for b in 0..ext.count {
            let block_idx = ext.start + b;
            read_block(block_idx, &mut buf);

            for e in 0..DIRENTS_PER_BLOCK {
                let offset = e * RAW_DIRENT_SIZE;
                let entry: RawDirEntry =
                    core::ptr::read_unaligned(buf[offset..].as_ptr() as *const RawDirEntry);
                if entry.is_free() {
                    // Écrire dans ce slot.
                    core::ptr::write_unaligned(buf[offset..].as_mut_ptr() as *mut RawDirEntry, raw);
                    write_block(block_idx, &buf);
                    dir_inode.size += RAW_DIRENT_SIZE as u64;
                    return Ok(());
                }
            }
        }
    }

    // Aucun slot libre — allouer un nouveau bloc.
    if dir_inode.n_extents as usize >= MAX_EXTENTS {
        return Err("kernfs: inode extent table full");
    }
    let new_block = alloc_blocks(1).ok_or("kernfs: no free blocks for dirent")?;
    let ext_idx = dir_inode.n_extents as usize;
    dir_inode.extents[ext_idx] = Extent {
        start: new_block,
        count: 1,
    };
    dir_inode.n_extents += 1;

    // Remplir le bloc de zéros puis écrire la première entrée.
    let mut blank = [0u8; BLOCK_SIZE];
    core::ptr::write_unaligned(blank.as_mut_ptr() as *mut RawDirEntry, raw);
    write_block(new_block, &blank);
    dir_inode.size += RAW_DIRENT_SIZE as u64;

    Ok(())
}

/// Supprime une entrée de répertoire en mettant `inode_nr` à 0.
///
/// # Safety
/// Lit/écrit un bloc.
unsafe fn remove_dirent(block_idx: u32, entry_idx: usize) {
    let mut buf = [0u8; BLOCK_SIZE];
    read_block(block_idx, &mut buf);
    let offset = entry_idx * RAW_DIRENT_SIZE;
    let zero = RawDirEntry {
        inode_nr: 0,
        kind: InodeKind::File,
        _pad: [0; 7],
        name: [0; 16],
    };
    core::ptr::write_unaligned(buf[offset..].as_mut_ptr() as *mut RawDirEntry, zero);
    write_block(block_idx, &buf);
}

// ---------------------------------------------------------------------------
// Résolution de chemin
// ---------------------------------------------------------------------------

/// Résout un chemin relatif depuis l'inode `start_nr` et retourne l'inode nr
/// du nœud cible.
///
/// Chemin "." → retourne `start_nr`.
/// Chemin vide → retourne `start_nr`.
///
/// # Safety
/// Appelle `find_dirent` → lit des blocs.
unsafe fn resolve_path(start_nr: u64, path: &str) -> Option<u64> {
    use super::path::PathComponents;

    let mut current_nr = start_nr;

    for component in PathComponents::new(path) {
        let dir = get_inode(current_nr)?;
        if dir.kind != InodeKind::Directory {
            return None;
        }
        let (_, _, entry) = find_dirent(dir, component)?;
        current_nr = entry.inode_nr;
    }
    Some(current_nr)
}

// ---------------------------------------------------------------------------
// Structure publique KernFs
// ---------------------------------------------------------------------------

/// Handle sur le filesystem KernFS.
///
/// C'est un ZST (zero-sized type) : tout l'état est dans `KERNFS_STATE`.
/// On n'a besoin que d'une instance pour l'implémenter en tant que `Filesystem`.
pub struct KernFs;

// ---------------------------------------------------------------------------
// Implémentation du trait Filesystem
// ---------------------------------------------------------------------------

impl Filesystem for KernFs {
    fn name(&self) -> &'static str {
        "kernfs"
    }

    // ── stat() ───────────────────────────────────────────────────────────────

    fn stat(&self, path: &str) -> Option<InodeMeta> {
        unsafe {
            let nr = resolve_path(ROOT_INODE, path)?;
            let inode = get_inode(nr)?;
            Some(InodeMeta {
                inode_nr: inode.nr,
                kind: inode.kind,
                size: inode.size,
                blocks: inode.total_blocks() as u64 * (BLOCK_SIZE as u64 / 512),
                mode: inode.mode,
                uid: inode.uid,
                gid: inode.gid,
            })
        }
    }

    // ── read() ───────────────────────────────────────────────────────────────

    fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, &'static str> {
        unsafe {
            let nr = resolve_path(ROOT_INODE, path).ok_or("kernfs: file not found")?;
            let inode = get_inode(nr).ok_or("kernfs: invalid inode")?;

            if inode.kind == InodeKind::Directory {
                return Err("kernfs: cannot read a directory");
            }

            let file_size = inode.size;
            if offset >= file_size {
                return Ok(0);
            }

            let to_read = buf.len().min((file_size - offset) as usize);
            let mut done = 0usize;
            let mut file_off = offset;

            // Parcourir les extents pour trouver les blocs correspondant à `offset`.
            let n_extents = inode.n_extents as usize;
            'outer: for ext_i in 0..n_extents {
                let ext = inode.extents[ext_i];
                let ext_bytes = ext.count as u64 * BLOCK_SIZE as u64;

                if file_off >= ext_bytes {
                    file_off -= ext_bytes;
                    continue;
                }

                // Position dans cet extent.
                let block_in_ext = (file_off / BLOCK_SIZE as u64) as u32;
                let byte_in_block = (file_off % BLOCK_SIZE as u64) as usize;

                for b in block_in_ext..ext.count {
                    let mut block_buf = [0u8; BLOCK_SIZE];
                    read_block(ext.start + b, &mut block_buf);

                    let start = if b == block_in_ext { byte_in_block } else { 0 };
                    let avail = BLOCK_SIZE - start;
                    let copy = avail.min(to_read - done);

                    buf[done..done + copy].copy_from_slice(&block_buf[start..start + copy]);
                    done += copy;
                    file_off += copy as u64;

                    if done >= to_read {
                        break 'outer;
                    }
                }
            }

            Ok(done)
        }
    }

    // ── write() ──────────────────────────────────────────────────────────────

    fn write(&mut self, path: &str, offset: u64, buf: &[u8]) -> Result<usize, &'static str> {
        unsafe {
            let nr = resolve_path(ROOT_INODE, path).ok_or("kernfs: file not found")?;
            let inode = get_inode(nr).ok_or("kernfs: invalid inode")?;

            if inode.kind == InodeKind::Directory {
                return Err("kernfs: cannot write to a directory");
            }

            let end = offset + buf.len() as u64;

            // Agrandir les extents si nécessaire.
            let current_cap = inode.total_blocks() as u64 * BLOCK_SIZE as u64;
            if end > current_cap {
                let needed_bytes = end - current_cap;
                let needed_blocks = needed_bytes.div_ceil(BLOCK_SIZE as u64) as u32;
                if inode.n_extents as usize >= MAX_EXTENTS {
                    return Err("kernfs: inode extent table full");
                }

                let new_start = alloc_blocks(needed_blocks).ok_or("kernfs: no free blocks")?;
                let ext_idx = inode.n_extents as usize;
                inode.extents[ext_idx] = Extent {
                    start: new_start,
                    count: needed_blocks,
                };
                inode.n_extents += 1;
            }

            // Écrire les données bloc par bloc.
            let mut done = 0usize;
            let mut file_off = offset;
            let to_write = buf.len();
            let n_extents = inode.n_extents as usize;

            'outer: for ext_i in 0..n_extents {
                let ext = inode.extents[ext_i];
                let ext_bytes = ext.count as u64 * BLOCK_SIZE as u64;

                if file_off >= ext_bytes {
                    file_off -= ext_bytes;
                    continue;
                }

                let block_in_ext = (file_off / BLOCK_SIZE as u64) as u32;
                let byte_in_block = (file_off % BLOCK_SIZE as u64) as usize;

                for b in block_in_ext..ext.count {
                    let mut block_buf = [0u8; BLOCK_SIZE];
                    // Lire avant d'écrire (read-modify-write) si écriture partielle.
                    read_block(ext.start + b, &mut block_buf);

                    let start = if b == block_in_ext { byte_in_block } else { 0 };
                    let avail = BLOCK_SIZE - start;
                    let copy = avail.min(to_write - done);

                    block_buf[start..start + copy].copy_from_slice(&buf[done..done + copy]);
                    write_block(ext.start + b, &block_buf);

                    done += copy;
                    file_off += copy as u64;

                    if done >= to_write {
                        break 'outer;
                    }
                }
            }

            // Mettre à jour la taille si on a écrit au-delà.
            if end > inode.size {
                inode.size = end;
            }

            Ok(done)
        }
    }

    // ── create() ─────────────────────────────────────────────────────────────

    fn create(&mut self, path: &str, kind: InodeKind, mode: u16) -> Result<(), &'static str> {
        unsafe {
            use super::path::{basename, dirname};

            let parent_path = dirname(path);
            let name = basename(path);

            if name.is_empty() || name.len() > 15 {
                return Err("kernfs: name too long (max 15 chars) or empty");
            }

            // Vérifier que le parent existe et est un répertoire.
            let parent_nr = resolve_path(ROOT_INODE, parent_path)
                .ok_or("kernfs: parent directory not found")?;
            let parent = get_inode(parent_nr).ok_or("kernfs: bad parent inode")?;
            if parent.kind != InodeKind::Directory {
                return Err("kernfs: parent is not a directory");
            }

            // Vérifier que le nom n'existe pas déjà.
            if find_dirent(parent, name).is_some() {
                return Err("kernfs: file already exists");
            }

            // Allouer un inode.
            let child_nr = alloc_inode(kind, mode).ok_or("kernfs: no free inodes")?;

            // Pour les répertoires, allouer un bloc pour "." et "..".
            if kind == InodeKind::Directory {
                let child = get_inode(child_nr).ok_or("kernfs: bad child inode")?;
                let blk = alloc_blocks(1).ok_or("kernfs: no free blocks for dir")?;
                child.extents[0] = Extent {
                    start: blk,
                    count: 1,
                };
                child.n_extents = 1;
                // Ajouter "." et ".."
                add_dirent(child, ".", child_nr, InodeKind::Directory)?;
                add_dirent(child, "..", parent_nr, InodeKind::Directory)?;
            }

            // Ajouter l'entrée dans le parent.
            // On relit le parent après les éventuelles modifications ci-dessus.
            let parent_mut = get_inode(parent_nr).ok_or("kernfs: bad parent inode")?;
            add_dirent(parent_mut, name, child_nr, kind)?;

            Ok(())
        }
    }

    // ── remove() ─────────────────────────────────────────────────────────────

    fn remove(&mut self, path: &str) -> Result<(), &'static str> {
        unsafe {
            use super::path::{basename, dirname};

            let parent_path = dirname(path);
            let name = basename(path);

            let parent_nr =
                resolve_path(ROOT_INODE, parent_path).ok_or("kernfs: parent not found")?;
            let parent = get_inode(parent_nr).ok_or("kernfs: bad parent")?;

            let (block_idx, entry_idx, entry) =
                find_dirent(parent, name).ok_or("kernfs: file not found")?;

            // Refuser de supprimer un répertoire non vide.
            if entry.kind == InodeKind::Directory {
                let child = get_inode(entry.inode_nr).ok_or("kernfs: bad child")?;
                // Un répertoire vide a seulement "." et ".." (2 entrées).
                let entries = (child.size / RAW_DIRENT_SIZE as u64) as usize;
                if entries > 2 {
                    return Err("kernfs: directory not empty");
                }
            }

            remove_dirent(block_idx, entry_idx);
            free_inode(entry.inode_nr);

            Ok(())
        }
    }

    // ── readdir() ────────────────────────────────────────────────────────────

    fn readdir(
        &self,
        path: &str,
        callback: &mut dyn FnMut(DirEntry) -> bool,
    ) -> Result<(), &'static str> {
        unsafe {
            let nr = resolve_path(ROOT_INODE, path).ok_or("kernfs: path not found")?;
            let inode = get_inode(nr).ok_or("kernfs: bad inode")?;
            if inode.kind != InodeKind::Directory {
                return Err("kernfs: not a directory");
            }

            let mut buf = [0u8; BLOCK_SIZE];
            let n_extents = inode.n_extents as usize;

            'outer: for ext_i in 0..n_extents {
                let ext = inode.extents[ext_i];
                for b in 0..ext.count {
                    read_block(ext.start + b, &mut buf);

                    for e in 0..DIRENTS_PER_BLOCK {
                        let offset = e * RAW_DIRENT_SIZE;
                        let raw: RawDirEntry =
                            core::ptr::read_unaligned(buf[offset..].as_ptr() as *const RawDirEntry);
                        if raw.is_free() {
                            continue;
                        }

                        // Construire un DirEntry VFS.
                        let mut vfs_entry = DirEntry {
                            inode_nr: raw.inode_nr,
                            kind: raw.kind,
                            name: [0u8; NAME_MAX + 1],
                        };
                        let nlen = raw
                            .name
                            .iter()
                            .position(|&b| b == 0)
                            .unwrap_or(16)
                            .min(NAME_MAX);
                        vfs_entry.name[..nlen].copy_from_slice(&raw.name[..nlen]);

                        if !callback(vfs_entry) {
                            break 'outer;
                        }
                    }
                }
            }
            Ok(())
        }
    }

    // ── truncate() ───────────────────────────────────────────────────────────

    fn truncate(&mut self, path: &str, new_size: u64) -> Result<(), &'static str> {
        unsafe {
            let nr = resolve_path(ROOT_INODE, path).ok_or("kernfs: file not found")?;
            let inode = get_inode(nr).ok_or("kernfs: bad inode")?;

            if new_size > inode.size {
                // Étendre : écrire des zéros à la fin.
                let zeros_needed = (new_size - inode.size) as usize;
                // On alloue un vecteur de zéros sur la pile (limité à 4 KiB).
                let chunk = BLOCK_SIZE.min(zeros_needed);
                let zeros = [0u8; BLOCK_SIZE];
                self.write(path, inode.size, &zeros[..chunk])?;
            } else if new_size < inode.size {
                // Réduire : libérer les blocs excédentaires.
                let needed_blocks = new_size.div_ceil(BLOCK_SIZE as u64) as u32;
                let n_extents = inode.n_extents as usize;
                let mut kept = 0u32;

                for ext_i in 0..n_extents {
                    let ext = &mut inode.extents[ext_i];
                    if ext.is_empty() {
                        continue;
                    }

                    if kept >= needed_blocks {
                        // Libérer cet extent entier.
                        free_blocks(ext.start, ext.count);
                        *ext = Extent { start: 0, count: 0 };
                    } else if kept + ext.count > needed_blocks {
                        // Libérer la fin de cet extent.
                        let keep = needed_blocks - kept;
                        free_blocks(ext.start + keep, ext.count - keep);
                        ext.count = keep;
                        kept += keep;
                    } else {
                        kept += ext.count;
                    }
                }
                // Recalculer n_extents.
                inode.n_extents = inode.extents.iter().filter(|e| !e.is_empty()).count() as u8;
                inode.size = new_size;
            }
            Ok(())
        }
    }

    // ── rename() ─────────────────────────────────────────────────────────────

    fn rename(&mut self, old_path: &str, new_path: &str) -> Result<(), &'static str> {
        unsafe {
            use super::path::{basename, dirname};

            let old_parent_path = dirname(old_path);
            let old_name = basename(old_path);
            let new_parent_path = dirname(new_path);
            let new_name = basename(new_path);

            if new_name.len() > 15 {
                return Err("kernfs: new name too long (max 15 chars)");
            }

            // Trouver et supprimer l'ancienne entrée.
            let old_parent_nr =
                resolve_path(ROOT_INODE, old_parent_path).ok_or("kernfs: old parent not found")?;
            let old_parent = get_inode(old_parent_nr).ok_or("kernfs: bad old parent")?;
            let (block_idx, entry_idx, old_entry) =
                find_dirent(old_parent, old_name).ok_or("kernfs: old file not found")?;

            remove_dirent(block_idx, entry_idx);

            // Ajouter la nouvelle entrée dans le nouveau parent.
            let new_parent_nr =
                resolve_path(ROOT_INODE, new_parent_path).ok_or("kernfs: new parent not found")?;
            let new_parent = get_inode(new_parent_nr).ok_or("kernfs: bad new parent")?;

            add_dirent(new_parent, new_name, old_entry.inode_nr, old_entry.kind)?;

            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Initialisation
// ---------------------------------------------------------------------------

/// Initialise KernFS : crée le répertoire racine et l'arborescence de base.
///
/// Doit être appelé après `block::init()`.
///
/// # Safety
/// Écrit dans `KERNFS_STATE` et sur le RAM disk.
pub unsafe fn init() -> KernFs {
    let state = &mut KERNFS_STATE;

    // ── Inode 1 : répertoire racine "/" ───────────────────────────────────────
    let root_block = alloc_blocks(1).expect("kernfs: cannot allocate root dir block");

    state.inodes[1] = KernInode {
        nr: ROOT_INODE,
        kind: InodeKind::Directory,
        size: 0,
        mode: 0o755,
        uid: 0,
        gid: 0,
        nlinks: 2,
        extents: [
            Extent {
                start: root_block,
                count: 1,
            },
            Extent { start: 0, count: 0 },
            Extent { start: 0, count: 0 },
            Extent { start: 0, count: 0 },
            Extent { start: 0, count: 0 },
            Extent { start: 0, count: 0 },
            Extent { start: 0, count: 0 },
            Extent { start: 0, count: 0 },
        ],
        n_extents: 1,
    };
    state.sb.free_inodes -= 1;

    // Zéro le bloc racine et écrire "." et "..".
    let blank = [0u8; BLOCK_SIZE];
    write_block(root_block, &blank);

    let mut fs = KernFs;

    // Ajouter "." et ".." dans la racine.
    {
        let root = get_inode(ROOT_INODE).unwrap();
        add_dirent(root, ".", ROOT_INODE, InodeKind::Directory).unwrap();
        add_dirent(root, "..", ROOT_INODE, InodeKind::Directory).unwrap();
    }

    // ── Arborescence standard ─────────────────────────────────────────────────
    for dir in &["bin", "etc", "tmp", "dev", "proc", "home"] {
        fs.create(dir, InodeKind::Directory, 0o755)
            .unwrap_or_else(|e| crate::kprintln!("[KERNFS] mkdir {} failed: {}", dir, e));
    }

    // Fichier de bienvenue.
    fs.create("etc/motd", InodeKind::File, 0o644).ok();
    fs.write("etc/motd", 0, b"Welcome to KernOS!\n").ok();

    state.ready = true;

    crate::kprintln!(
        "[KERNFS] init — {} blocks free / {}  |  {} inodes free / {}",
        state.sb.free_blocks,
        state.sb.total_blocks,
        state.sb.free_inodes,
        state.sb.total_inodes,
    );

    fs
}
