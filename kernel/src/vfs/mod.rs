// kernel/src/vfs/mod.rs
//
// Virtual File System — Brick 8, part 1/3.
//
// ── Rôle de la couche VFS ─────────────────────────────────────────────────────
//
// La VFS est une couche d'abstraction qui permet au reste du kernel (shell,
// syscalls, réseau) d'accéder aux fichiers sans savoir sur quel filesystem
// ils se trouvent.
//
// Au lieu d'appeler `kernfs::read_file(...)` directement, le code appelle
// `vfs::open(path)` qui retourne un `FileHandle` générique.  La VFS se charge
// de trouver le filesystem monté sur ce chemin et de déléguer l'appel.
//
// ── Architecture ──────────────────────────────────────────────────────────────
//
//   vfs::open("/etc/passwd")
//     → cherche le montage correspondant au chemin
//     → appelle filesystem.open("etc/passwd")
//     → retourne FileHandle { inode, position, fs_ref }
//
// ── Traits ────────────────────────────────────────────────────────────────────
//
//   Filesystem  — implémenté par KernFS (et plus tard ext4, procfs, devfs…)
//   FileHandle  — curseur de lecture/écriture dans un fichier ouvert
//
// ── Table de montage ──────────────────────────────────────────────────────────
//
//   MOUNT_TABLE  — tableau statique de (chemin, &dyn Filesystem)
//   vfs::mount() — enregistre un filesystem sous un chemin
//   vfs::open()  — résout le chemin et délègue au bon filesystem

#![allow(dead_code)]
#![allow(static_mut_refs)]

pub mod kernfs;
pub mod path;

// ---------------------------------------------------------------------------
// Constantes globales VFS
// ---------------------------------------------------------------------------

/// Taille maximale d'un nom de fichier ou répertoire (sans le '\0').
pub const NAME_MAX: usize = 255;

/// Taille maximale d'un chemin absolu.
pub const PATH_MAX: usize = 4096;

/// Nombre maximum de filesystems montés simultanément.
const MAX_MOUNTS: usize = 16;

/// Nombre maximum de fichiers ouverts simultanément (tous processus confondus).
pub const MAX_OPEN_FILES: usize = 256;

// ---------------------------------------------------------------------------
// Types de nœuds (inode kinds)
// ---------------------------------------------------------------------------

/// Le type d'un inode — fichier, répertoire, lien symbolique, ou device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InodeKind {
    /// Fichier ordinaire contenant des données.
    File = 0,
    /// Répertoire contenant des entrées (nom → inode).
    Directory = 1,
    /// Lien symbolique (stocke le chemin cible comme données).
    Symlink = 2,
    /// Device caractère ou bloc (Brick 9+).
    Device = 3,
}

// ---------------------------------------------------------------------------
// Métadonnées d'un inode (vue VFS — indépendante du FS)
// ---------------------------------------------------------------------------

/// Métadonnées d'un fichier/répertoire vues par la couche VFS.
///
/// Chaque filesystem remplit cette structure quand la VFS lui demande les
/// informations d'un inode.  C'est l'équivalent du `struct stat` POSIX.
#[derive(Debug, Clone, Copy)]
pub struct InodeMeta {
    /// Numéro d'inode unique dans son filesystem.
    pub inode_nr: u64,
    /// Type du nœud (fichier, répertoire, …).
    pub kind: InodeKind,
    /// Taille du contenu en octets.
    pub size: u64,
    /// Nombre de blocs de 512 octets occupés (pour compatibilité POSIX stat).
    pub blocks: u64,
    /// Permissions UNIX (bits rwxrwxrwx + setuid/setgid/sticky).
    pub mode: u16,
    /// Propriétaire (user ID).
    pub uid: u32,
    /// Groupe (group ID).
    pub gid: u32,
}

// ---------------------------------------------------------------------------
// Entrée de répertoire
// ---------------------------------------------------------------------------

/// Une entrée dans un répertoire : association (nom → inode).
///
/// Retournée par `Filesystem::readdir()`.
#[derive(Debug, Clone, Copy)]
pub struct DirEntry {
    /// Numéro d'inode pointé par cette entrée.
    pub inode_nr: u64,
    /// Type du nœud (optimisation : évite un stat() supplémentaire).
    pub kind: InodeKind,
    /// Nom du fichier ou répertoire (tableau fixe, terminé par '\0').
    pub name: [u8; NAME_MAX + 1],
}

impl DirEntry {
    /// Retourne le nom comme slice de bytes (sans le '\0' terminal).
    pub fn name_bytes(&self) -> &[u8] {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(NAME_MAX);
        &self.name[..len]
    }

    /// Retourne le nom comme `&str` (UTF-8, sans le '\0').
    pub fn name_str(&self) -> &str {
        core::str::from_utf8(self.name_bytes()).unwrap_or("<invalid>")
    }
}

// ---------------------------------------------------------------------------
// Trait Filesystem
// ---------------------------------------------------------------------------

/// Interface qu'un filesystem doit implémenter pour être monté dans la VFS.
///
/// Toutes les méthodes reçoivent des chemins *relatifs* à la racine du
/// filesystem (la VFS a déjà résolu le point de montage).
pub trait Filesystem: Send + Sync {
    /// Nom du filesystem (ex : "kernfs", "ext2").  Pour le débogage.
    fn name(&self) -> &'static str;

    /// Retourne les métadonnées d'un fichier/répertoire identifié par chemin.
    ///
    /// Retourne `None` si le fichier n'existe pas.
    fn stat(&self, path: &str) -> Option<InodeMeta>;

    /// Lit au plus `buf.len()` octets du fichier à `path`, à partir de
    /// `offset`.  Retourne le nombre d'octets effectivement lus.
    ///
    /// Retourne `Err` si le fichier n'existe pas ou si `path` est un répertoire.
    fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, &'static str>;

    /// Écrit `buf` dans le fichier à `path`, à partir de `offset`.
    /// Crée le fichier s'il n'existe pas.
    /// Retourne le nombre d'octets écrits.
    fn write(&mut self, path: &str, offset: u64, buf: &[u8]) -> Result<usize, &'static str>;

    /// Crée un fichier vide à `path`.
    /// Retourne `Err` si le fichier existe déjà ou si le répertoire parent
    /// n'existe pas.
    fn create(&mut self, path: &str, kind: InodeKind, mode: u16) -> Result<(), &'static str>;

    /// Supprime un fichier ou répertoire vide.
    fn remove(&mut self, path: &str) -> Result<(), &'static str>;

    /// Liste les entrées d'un répertoire.
    /// Appelle `callback` pour chaque entrée.  Si `callback` retourne `false`,
    /// l'itération s'arrête.
    fn readdir(
        &self,
        path: &str,
        callback: &mut dyn FnMut(DirEntry) -> bool,
    ) -> Result<(), &'static str>;

    /// Tronque ou étend un fichier à exactement `new_size` octets.
    fn truncate(&mut self, path: &str, new_size: u64) -> Result<(), &'static str>;

    /// Renomme / déplace un fichier.
    fn rename(&mut self, old_path: &str, new_path: &str) -> Result<(), &'static str>;
}

// ---------------------------------------------------------------------------
// Table de montage
// ---------------------------------------------------------------------------

/// Une entrée dans la table de montage.
struct MountEntry {
    /// Point de montage (ex : "/", "/tmp", "/dev").
    /// Stocké comme tableau fixe pour éviter les allocations.
    mountpoint: [u8; PATH_MAX],
    /// Longueur utile de `mountpoint`.
    mp_len: usize,
    /// Pointeur vers le filesystem monté.
    /// `*mut` parce que `write()` etc. prennent `&mut self`.
    ///
    /// # Safety
    /// Ce pointeur est valide tant que le filesystem est monté.
    /// La VFS est single-threadée à ce stade.
    fs: *mut dyn Filesystem,
}

/// La table globale de montage.
static mut MOUNT_TABLE: [Option<MountEntry>; MAX_MOUNTS] = [const { None }; MAX_MOUNTS];

/// Nombre de filesystems actuellement montés.
static mut MOUNT_COUNT: usize = 0;

// ---------------------------------------------------------------------------
// API de montage
// ---------------------------------------------------------------------------

/// Monte un filesystem sur `mountpoint`.
///
/// `fs` doit être une référence vers un filesystem alloué de façon stable
/// (static ou heap).  Elle doit rester valide tant que le filesystem est monté.
///
/// # Panics
/// Panique si `MAX_MOUNTS` est dépassé.
///
/// # Safety
/// Écrit dans `static mut MOUNT_TABLE`.  Single-threadé uniquement.
pub unsafe fn mount(mountpoint: &str, fs: *mut dyn Filesystem) {
    assert!(
        MOUNT_COUNT < MAX_MOUNTS,
        "vfs: MAX_MOUNTS ({}) exceeded",
        MAX_MOUNTS
    );

    let mut mp_bytes = [0u8; PATH_MAX];
    let len = mountpoint.len().min(PATH_MAX - 1);
    mp_bytes[..len].copy_from_slice(&mountpoint.as_bytes()[..len]);

    MOUNT_TABLE[MOUNT_COUNT] = Some(MountEntry {
        mountpoint: mp_bytes,
        mp_len: len,
        fs,
    });
    MOUNT_COUNT += 1;

    crate::kprintln!("[VFS]  mounted '{}' on '{}'", (*fs).name(), mountpoint);
}

/// Retourne le filesystem monté le plus précis pour `path` et le sous-chemin
/// relatif correspondant.
///
/// Exemple : si "/" est monté sur KernFS et "/proc" sur ProcFS,
///   `resolve("/proc/cpuinfo")` → (&mut ProcFS, "cpuinfo")
///
/// # Safety
/// Lit `static mut MOUNT_TABLE`.
unsafe fn resolve(path: &str) -> Option<(&'static mut dyn Filesystem, &'static str)> {
    // Trouver le point de montage avec le plus long préfixe commun.
    let mut best_len: usize = 0;
    let mut best_idx: Option<usize> = None;

    for (i, entry) in MOUNT_TABLE[..MOUNT_COUNT].iter().enumerate() {
        if let Some(e) = entry {
            let mp = core::str::from_utf8(&e.mountpoint[..e.mp_len]).unwrap_or("");
            if path.starts_with(mp) && e.mp_len >= best_len {
                best_len = e.mp_len;
                best_idx = Some(i);
            }
        }
    }

    let idx = best_idx?;
    let entry = MOUNT_TABLE[idx].as_ref()?;
    let fs = &mut *entry.fs;

    // Calcul du sous-chemin relatif au point de montage.
    let mp = core::str::from_utf8(&entry.mountpoint[..entry.mp_len]).unwrap_or("");
    let rel = if path.len() == mp.len() {
        // On a demandé exactement le point de montage → chemin relatif = "."
        "."
    } else {
        // Retirer le préfixe du point de montage (et le '/' suivant si présent).
        let rest = &path[mp.len()..];
        rest.trim_start_matches('/')
    };

    // Transmettre la durée de vie 'static — valide car les filesystems
    // sont des statics ou des allocations heap qui ne bougent pas.
    let fs_ref: &'static mut dyn Filesystem = core::mem::transmute(fs);
    let rel_ref: &'static str = core::mem::transmute(rel);

    Some((fs_ref, rel_ref))
}

// ---------------------------------------------------------------------------
// API VFS publique
// ---------------------------------------------------------------------------

/// Retourne les métadonnées d'un fichier/répertoire.
pub fn stat(path: &str) -> Option<InodeMeta> {
    unsafe {
        let (fs, rel) = resolve(path)?;
        fs.stat(rel)
    }
}

/// Lit des données d'un fichier.
///
/// Lit au plus `buf.len()` octets à partir de `offset`.
/// Retourne le nombre d'octets lus.
pub fn read(path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, &'static str> {
    unsafe {
        let (fs, rel) = resolve(path).ok_or("vfs: path not found")?;
        fs.read(rel, offset, buf)
    }
}

/// Écrit des données dans un fichier (le crée si nécessaire).
pub fn write(path: &str, offset: u64, buf: &[u8]) -> Result<usize, &'static str> {
    unsafe {
        let (fs, rel) = resolve(path).ok_or("vfs: path not found")?;
        fs.write(rel, offset, buf)
    }
}

/// Crée un fichier ou répertoire.
pub fn create(path: &str, kind: InodeKind, mode: u16) -> Result<(), &'static str> {
    unsafe {
        let (fs, rel) = resolve(path).ok_or("vfs: path not found")?;
        fs.create(rel, kind, mode)
    }
}

/// Supprime un fichier ou répertoire vide.
pub fn remove(path: &str) -> Result<(), &'static str> {
    unsafe {
        let (fs, rel) = resolve(path).ok_or("vfs: path not found")?;
        fs.remove(rel)
    }
}

/// Liste les entrées d'un répertoire.
///
/// Appelle `callback(entry)` pour chaque entrée.
/// Si `callback` retourne `false`, l'itération s'arrête.
pub fn readdir(path: &str, callback: &mut dyn FnMut(DirEntry) -> bool) -> Result<(), &'static str> {
    unsafe {
        let (fs, rel) = resolve(path).ok_or("vfs: path not found")?;
        fs.readdir(rel, callback)
    }
}

/// Renomme ou déplace un fichier.
pub fn rename(old_path: &str, new_path: &str) -> Result<(), &'static str> {
    unsafe {
        let (fs, old_rel) = resolve(old_path).ok_or("vfs: old path not found")?;
        // Pour un rename inter-filesystem, il faudrait copier + supprimer.
        // On suppose pour l'instant que les deux chemins sont sur le même FS.
        let new_rel = path::relative_to(new_path, "").trim_start_matches('/');
        fs.rename(old_rel, new_rel)
    }
}

/// Tronque un fichier à `new_size` octets.
pub fn truncate(path: &str, new_size: u64) -> Result<(), &'static str> {
    unsafe {
        let (fs, rel) = resolve(path).ok_or("vfs: path not found")?;
        fs.truncate(rel, new_size)
    }
}
