// Chaîne de rendu : dépaquetage du format source, correction colorimétrique,
// puis effets CRT optionnels. Tout tient en une passe — seule la halation aurait
// besoin d'une image floutée complète, et on l'approche par quelques prises de
// voisinage plutôt que par un aller-retour en mémoire.

struct Uniforms {
    // Taille de l'image source, en pixels.
    src_size: vec2<f32>,
    // Taille de la zone d'affichage, en pixels.
    view_size: vec2<f32>,

    brightness: f32,
    contrast: f32,
    saturation: f32,
    gamma: f32,

    scanline: f32,
    mask: f32,
    curvature: f32,
    halation: f32,

    flags: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

const FLAG_YUYV: u32         = 1u;
const FLAG_BT709: u32        = 2u;
const FLAG_FULL_RANGE: u32   = 4u;
const FLAG_CRT: u32          = 8u;
const FLAG_SMOOTH: u32       = 16u;
const FLAG_SRGB_SURFACE: u32 = 32u;
const FLAG_GREY: u32         = 64u;
const FLAG_UYVY: u32         = 128u;
const FLAG_NV12: u32         = 256u;

const PI: f32 = 3.14159265;

// Décalage du faisceau dans sa ligne source, en fraction de ligne.
//
// Centré au milieu de la ligne (0,5), il tombe pile entre les deux pixels
// d'écran d'un agrandissement ×2 — celui d'une source 480p sur un écran
// 1080p : les deux en reçoivent exactement la même part et l'image est
// assombrie sans montrer la moindre scanline. Un quart de ligne plus haut, le
// faisceau tient entier dans le premier des deux pixels : une ligne claire,
// une ligne sombre.
const BEAM_OFFSET: f32 = 0.25;

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Triangle unique couvrant toute la zone : pas de tampon de sommets, pas
// d'assemblage, et la zone est délimitée par le viewport côté Rust.
@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    var out: VsOut;
    out.uv = vec2<f32>(x, y);
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

fn has(flag: u32) -> bool {
    return (u.flags & flag) != 0u;
}

// Conversion YUV → RGB.
//
// Deux paramètres décident du résultat et se trompent silencieusement : la
// matrice (BT.601 en définition standard, BT.709 au-delà) et la plage. Un signal
// vidéo n'utilise que 16..235 ; le prendre pour du 0..255 délave les noirs.
fn yuv_to_rgb(luma: f32, cb_in: f32, cr_in: f32) -> vec3<f32> {
    var y = luma;
    var cb = cb_in - 0.5;
    var cr = cr_in - 0.5;

    if (!has(FLAG_FULL_RANGE)) {
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        cb = cb * (255.0 / 224.0);
        cr = cr * (255.0 / 224.0);
    }

    var rgb: vec3<f32>;
    if (has(FLAG_BT709)) {
        rgb = vec3<f32>(
            y + 1.5748 * cr,
            y - 0.1873 * cb - 0.4681 * cr,
            y + 1.8556 * cb,
        );
    } else {
        rgb = vec3<f32>(
            y + 1.4020 * cr,
            y - 0.344136 * cb - 0.714136 * cr,
            y + 1.7720 * cb,
        );
    }
    return clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
}

// Lit un pixel source, dans la disposition où l'appareil l'a produit — aucun de
// ces dépaquetages n'a coûté un cycle de CPU.
//
// Le YUYV et l'UYVY sont stockés en texture RGBA de demi-largeur : un texel
// porte deux pixels voisins qui partagent leur chrominance. Le NV12 est
// planaire ; sa texture est une bande d'octets où le plan de chrominance,
// entrelacé et de demi-résolution, est posé sous le plan de luminance.
fn fetch(ix: i32, iy: i32) -> vec3<f32> {
    let w = i32(u.src_size.x);
    let h = i32(u.src_size.y);
    let x = clamp(ix, 0, w - 1);
    let y = clamp(iy, 0, h - 1);

    if (has(FLAG_GREY)) {
        // Luminance seule : on la recopie sur les trois canaux.
        return vec3<f32>(textureLoad(src, vec2<i32>(x, y), 0).r);
    }

    if (has(FLAG_YUYV)) {
        // (Y0, U, Y1, V)
        let texel = textureLoad(src, vec2<i32>(x / 2, y), 0);
        var luma = texel.r;
        if ((x & 1) == 1) {
            luma = texel.b;
        }
        return yuv_to_rgb(luma, texel.g, texel.a);
    }

    if (has(FLAG_UYVY)) {
        // (U, Y0, V, Y1) — mêmes octets, ordre inverse.
        let texel = textureLoad(src, vec2<i32>(x / 2, y), 0);
        var luma = texel.g;
        if ((x & 1) == 1) {
            luma = texel.a;
        }
        return yuv_to_rgb(luma, texel.r, texel.b);
    }

    if (has(FLAG_NV12)) {
        let luma = textureLoad(src, vec2<i32>(x, y), 0).r;
        // Une paire (Cb, Cr) pour quatre pixels : on retombe sur le début de la
        // paire, puis on descend dans le plan de chrominance.
        let cx = (x / 2) * 2;
        let cy = h + y / 2;
        let cb = textureLoad(src, vec2<i32>(cx, cy), 0).r;
        let cr = textureLoad(src, vec2<i32>(min(cx + 1, w - 1), cy), 0).r;
        return yuv_to_rgb(luma, cb, cr);
    }

    return textureLoad(src, vec2<i32>(x, y), 0).rgb;
}

// Échantillonnage. En agrandissement entier on veut du plus-proche-voisin : des
// pixels carrés et nets, pas du flou d'interpolation. Le bilinéaire n'est là que
// pour les facteurs non entiers, où le plus proche voisin donnerait des lignes
// de pixels irrégulièrement dupliquées.
//
// Il est fait à la main plutôt que par un échantillonneur matériel, parce
// qu'interpoler linéairement une texture YUYV mélangerait les luminances de deux
// pixels avec la mauvaise chrominance.
fn sample_source(uv: vec2<f32>) -> vec3<f32> {
    let p = uv * u.src_size - vec2<f32>(0.5);

    if (!has(FLAG_SMOOTH)) {
        return fetch(i32(floor(uv.x * u.src_size.x)), i32(floor(uv.y * u.src_size.y)));
    }

    let base = floor(p);
    let f = p - base;
    let i0 = vec2<i32>(base);

    let c00 = fetch(i0.x, i0.y);
    let c10 = fetch(i0.x + 1, i0.y);
    let c01 = fetch(i0.x, i0.y + 1);
    let c11 = fetch(i0.x + 1, i0.y + 1);

    return mix(mix(c00, c10, f.x), mix(c01, c11, f.x), f.y);
}

fn correct_color(input: vec3<f32>) -> vec3<f32> {
    var c = input;

    // Gamma d'abord : c'est une remise en forme de la courbe de transfert, elle
    // doit précéder les ajustements de niveau pour ne pas les déformer.
    c = pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / max(u.gamma, 0.01)));

    // Contraste autour du gris moyen, puis décalage de luminosité.
    c = (c - 0.5) * u.contrast + 0.5 + u.brightness;

    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    c = mix(vec3<f32>(luma), c, u.saturation);

    return clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
}

// Courbure de dalle : on bombe les coordonnées avant l'échantillonnage.
fn curve(uv: vec2<f32>) -> vec2<f32> {
    var p = uv * 2.0 - 1.0;
    let r2 = dot(p, p);
    p = p * (1.0 + u.curvature * r2);
    return p * 0.5 + 0.5;
}

// Combien de lignes source tiennent dans un pixel d'écran, à cet endroit.
//
// C'est la dérivée verticale de `curve` — d/dy de y(1 + k(x² + 3y²)) — mise à
// l'échelle du rapport source/affichage. On la calcule plutôt que de la
// mesurer avec `fwidth` : les dérivées matérielles ne sont pas définies après
// le rejet des pixels hors dalle, qui dépend de la position.
fn line_density(uv: vec2<f32>) -> f32 {
    let p = uv * 2.0 - 1.0;
    let stretch = 1.0 + u.curvature * (p.x * p.x + 3.0 * p.y * p.y);
    return u.src_size.y * stretch / max(u.view_size.y, 1.0);
}

// Primitive de sin(π·x), en unités de ligne source.
fn beam_integral(x: f32) -> f32 {
    let whole = floor(x);
    return whole * (2.0 / PI) + (1.0 - cos(PI * (x - whole))) / PI;
}

// Profil du faisceau, moyenné sur la hauteur que le pixel couvre réellement.
//
// Prendre un point de la sinusoïde suppose qu'une ligne source occupe
// plusieurs pixels. Dès qu'elle en occupe moins d'un — c'est le cas partout en
// agrandissement ×1, et localement dès qu'on bombe la dalle, la courbure
// comprimant les lignes vers les bords — le motif bat sous le pas
// d'échantillonnage et donne de larges franges de moiré au lieu de scanlines.
// La moyenne sur l'empreinte du pixel est la valeur juste : le motif s'efface
// alors de lui-même vers sa moyenne (2/π) là où l'écran ne peut pas le rendre,
// au lieu de battre.
fn beam_profile(line: f32, density: f32) -> f32 {
    let half = 0.5 * max(density, 1e-4);
    let lo = line - half;
    let hi = line + half;
    // Les bornes sont ramenées près de zéro : `line` monte à la hauteur de la
    // source et la différence de deux primitives voisines y perdrait ses
    // décimales en f32.
    let base = floor(lo);
    return (beam_integral(hi - base) - beam_integral(lo - base)) / (hi - lo);
}

// Halation : la lumière des zones claires débordait sur le phosphore voisin.
// Quatre prises suffisent à l'évoquer sans passe de flou séparée.
fn halation_glow(uv: vec2<f32>) -> vec3<f32> {
    let step = 2.0 / u.src_size;
    var sum = vec3<f32>(0.0);
    sum += sample_source(uv + vec2<f32>(step.x, 0.0));
    sum += sample_source(uv - vec2<f32>(step.x, 0.0));
    sum += sample_source(uv + vec2<f32>(0.0, step.y));
    sum += sample_source(uv - vec2<f32>(0.0, step.y));
    return sum * 0.25;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    var uv = in.uv;
    // Mesurée sur les coordonnées d'écran, avant le rejet du hors-dalle.
    let density = line_density(in.uv);

    if (has(FLAG_CRT)) {
        uv = curve(uv);
        // Hors dalle : noir franc, comme le cadre d'un tube.
        if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
            return vec4<f32>(0.0, 0.0, 0.0, 1.0);
        }
    }

    var c = correct_color(sample_source(uv));

    if (has(FLAG_CRT)) {
        if (u.halation > 0.0) {
            let glow = correct_color(halation_glow(uv));
            c += u.halation * max(glow - vec3<f32>(0.5), vec3<f32>(0.0));
        }

        // Lignes de balayage, indexées sur les lignes de la source et non sur
        // les pixels de l'écran : elles restent stables quand on redimensionne.
        let line = uv.y * u.src_size.y + BEAM_OFFSET;
        c *= mix(1.0, beam_profile(line, density), u.scanline);

        // Masque d'ouverture : triade RGB sur les colonnes physiques de l'écran.
        let column = u32(in.pos.x) % 3u;
        var triad = vec3<f32>(1.0);
        if (column == 0u) {
            triad = vec3<f32>(1.0, 1.0 - u.mask, 1.0 - u.mask);
        } else if (column == 1u) {
            triad = vec3<f32>(1.0 - u.mask, 1.0, 1.0 - u.mask);
        } else {
            triad = vec3<f32>(1.0 - u.mask, 1.0 - u.mask, 1.0);
        }
        c *= triad;

        // Scanlines et masque mangent de la lumière ; on rend le gain perdu,
        // sinon activer le filtre revient à baisser la luminosité.
        c *= 1.0 + 0.6 * u.scanline + 0.5 * u.mask;
        c = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    }

    // Sur une surface sRGB, le matériel réencodera ce qu'on écrit. On annule
    // cette conversion : nos valeurs sont déjà dans l'espace d'affichage, et
    // les laisser passer éclaircirait toute l'image.
    if (has(FLAG_SRGB_SURFACE)) {
        c = pow(c, vec3<f32>(2.2));
    }

    return vec4<f32>(c, 1.0);
}
