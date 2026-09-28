# Fabrication des binaires de release.
#
#   make release      les deux binaires dans dist/, avec leurs sommes
#   make linux        l'un ou l'autre seulement
#   make windows
#   make clean-dist
#
# Prérequis, une fois :
#
#   sudo pacman -S --needed mingw-w64-gcc docker
#   cargo install cross
#   rustup target add x86_64-pc-windows-gnu
#
# Et le démon Docker démarré, pour la cible Linux seulement :
#
#   sudo systemctl start docker

VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DIST    := dist
LINUX   := x86_64-unknown-linux-gnu
# Chemin *dans le conteneur*, transmis via le passthrough de Cross.toml : sans
# ça bindgen tombe sur le libclang 3.8 préinstallé, qui est trop vieux.
LIBCLANG := /usr/lib/llvm-8/lib
WINDOWS := x86_64-pc-windows-gnu

.PHONY: release linux windows clean-dist

release: linux windows
	cd $(DIST) && sha256sum vidio vidio.exe > SHA256SUMS
	@echo
	@ls -l $(DIST)
	@echo
	@echo "VidIO $(VERSION) — à attacher à la release."
	@echo "glibc minimale exigée par le binaire Linux :"
	@objdump -T $(DIST)/vidio | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1

# Dans un conteneur, et c'est indispensable : il faut un libasound compilé
# contre une vieille glibc, pas seulement de vieux stubs. Celui d'Arch réclame
# des symboles GLIBC_2.43, ce qui interdit tout binaire portable lié ici —
# c'est sur cet écueil que la tentative avec zig a buté.
linux: | $(DIST)
	LIBCLANG_PATH=$(LIBCLANG) cross build --release --target $(LINUX)
	install -m755 target/$(LINUX)/release/vidio $(DIST)/vidio

# Pas de conteneur ici : mingw est installé en local et suffit. La cible GNU
# produit un exécutable qui ne réclame que des DLL système — ni runtime mingw,
# ni redistribuable Visual C++. Une minute vingt, contre plusieurs pour une
# image Docker à télécharger.
windows: | $(DIST)
	cargo build --release --target $(WINDOWS)
	install -m755 target/$(WINDOWS)/release/vidio.exe $(DIST)/vidio.exe

$(DIST):
	mkdir -p $(DIST)

clean-dist:
	rm -rf $(DIST)
