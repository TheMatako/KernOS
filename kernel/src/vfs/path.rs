// kernel/src/vfs/path.rs
//
// Utilitaires de manipulation de chemins — Brick 8, part 2/3.
//
// Pas d'allocation heap : tout travaille sur des slices &str statiques
// ou des tableaux sur la pile.

#![allow(dead_code)]
#![allow(static_mut_refs)]

use super::PATH_MAX;

// ---------------------------------------------------------------------------
// Itérateur de composantes de chemin
// ---------------------------------------------------------------------------

/// Itère sur les composantes d'un chemin UNIX, en ignorant les '/' multiples
/// et les composantes "." (point courant).
///
/// Exemple : "/foo//bar/./baz" → ["foo", "bar", "baz"]
pub struct PathComponents<'a> {
    remaining: &'a str,
}

impl<'a> PathComponents<'a> {
    pub fn new(path: &'a str) -> Self {
        // Retirer le '/' initial pour les chemins absolus.
        Self {
            remaining: path.trim_start_matches('/'),
        }
    }
}

impl<'a> Iterator for PathComponents<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Plus rien à parcourir.
            if self.remaining.is_empty() {
                return None;
            }

            // Trouver la prochaine composante (jusqu'au prochain '/').
            let (component, rest) = match self.remaining.find('/') {
                Some(pos) => (&self.remaining[..pos], &self.remaining[pos + 1..]),
                None => (self.remaining, ""),
            };

            self.remaining = rest.trim_start_matches('/');

            // Ignorer les composantes vides et ".".
            if component.is_empty() || component == "." {
                continue;
            }

            return Some(component);
        }
    }
}

// ---------------------------------------------------------------------------
// Fonctions utilitaires
// ---------------------------------------------------------------------------

/// Retourne le nom du fichier (dernière composante du chemin).
///
/// Exemple : `basename("/foo/bar/baz.txt")` → `"baz.txt"`
/// Exemple : `basename("/")` → `""`
pub fn basename(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// Retourne le répertoire parent d'un chemin.
///
/// Exemple : `dirname("/foo/bar/baz.txt")` → `"/foo/bar"`
/// Exemple : `dirname("/foo")` → `"/"`
/// Exemple : `dirname("/")` → `"/"`
pub fn dirname(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        None => ".",
        Some(0) => "/",
        Some(pos) => &trimmed[..pos],
    }
}

/// Retourne le chemin `path` relatif à `base`.
///
/// Si `path` commence par `base`, retire le préfixe.
/// Sinon retourne `path` intact.
pub fn relative_to<'a>(path: &'a str, base: &str) -> &'a str {
    if base.is_empty() || base == "/" {
        return path.trim_start_matches('/');
    }
    if let Some(rest) = path.strip_prefix(base) {
        return rest.trim_start_matches('/');
    }
    path
}

/// Vérifie qu'un nom de composante est valide (pas de '/', pas de '\0',
/// longueur ≤ NAME_MAX).
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= super::NAME_MAX
        && !name.contains('/')
        && !name.contains('\0')
        && name != ".."
        && name != "."
}

/// Résout les ".." dans un chemin absolu (normalisation).
///
/// Écrit le résultat dans `out` (tableau sur la pile, taille PATH_MAX).
/// Retourne la longueur du chemin normalisé.
///
/// Exemple : `"/foo/../bar/./baz"` → `"/bar/baz"`
pub fn normalize<'a>(path: &str, out: &'a mut [u8; PATH_MAX]) -> &'a str {
    // On garde une pile de composantes valides.
    // Chaque composante est une slice dans `path` (lifetime 'a = path).
    let mut stack: [&str; 64] = [""; 64];
    let mut depth: usize = 0;

    for component in PathComponents::new(path) {
        if component == ".." {
            depth = depth.saturating_sub(1);
        } else {
            if depth < 64 {
                stack[depth] = component;
                depth += 1;
            }
        }
    }

    // Reconstituer le chemin normalisé dans `out`.
    out[0] = b'/';
    let mut pos: usize = 1;

    for (i, comp) in stack[..depth].iter().enumerate() {
        let bytes = comp.as_bytes();
        if pos + bytes.len() + 1 > PATH_MAX {
            break;
        }
        out[pos..pos + bytes.len()].copy_from_slice(bytes);
        pos += bytes.len();
        if i + 1 < depth {
            out[pos] = b'/';
            pos += 1;
        }
    }

    // Chemin racine seul : longueur = 1.
    if depth == 0 {
        pos = 1;
    }

    core::str::from_utf8(&out[..pos]).unwrap_or("/")
}

/// Concatène `base` et `name` en un seul chemin, en insérant un '/' si besoin.
///
/// Écrit dans `out`.  Retourne la longueur du résultat.
pub fn join<'a>(base: &str, name: &str, out: &'a mut [u8; PATH_MAX]) -> &'a str {
    let base = base.trim_end_matches('/');
    let mut pos = 0usize;

    // Copier base.
    let blen = base.len().min(PATH_MAX - 2);
    out[..blen].copy_from_slice(&base.as_bytes()[..blen]);
    pos += blen;

    // Séparateur.
    if pos < PATH_MAX {
        out[pos] = b'/';
        pos += 1;
    }

    // Copier name.
    let nlen = name.len().min(PATH_MAX - pos);
    out[pos..pos + nlen].copy_from_slice(&name.as_bytes()[..nlen]);
    pos += nlen;

    core::str::from_utf8(&out[..pos]).unwrap_or("/")
}
