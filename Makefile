.PHONY: installer FORCE
installer: build/aldis-installer.sh build/aldis-installer.sh.sha256

build/aldis-installer.sh: installer/kpi.sh installer/installer.sh FORCE
	mkdir -p build
	{ \
	  cat installer/kpi.sh; \
	  echo; \
	  cat installer/installer.sh; \
	  echo; \
	  echo 'aldis_main "$$@"'; \
	} > build/aldis-installer.sh
	chmod +x build/aldis-installer.sh

build/aldis-installer.sh.sha256: build/aldis-installer.sh
	cd build && shasum -a 256 aldis-installer.sh > aldis-installer.sh.sha256

FORCE:
