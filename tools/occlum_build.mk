SGX_SDK ?= /opt/occlum/sgxsdk-tools

IMAGE := $(instance_dir)/image
SECURE_IMAGE := $(instance_dir)/build/mount/__ROOT/metadata
JSON_CONF := $(instance_dir)/Occlum.json

LIBOS := $(instance_dir)/build/lib/$(libos_lib).$(occlum_version)
SIGNED_ENCLAVE := $(instance_dir)/build/lib/libocclum-libos.signed.so

SEFS_CLI_SIM := $(occlum_dir)/build/bin/sefs-cli_sim
SIGNED_SEFS_CLI_LIB := $(occlum_dir)/build/lib/libsefs-cli.signed.so

BIN_LINKS := occlum_exec_client occlum_exec_server occlum-run
BIN_LINKS := $(addprefix $(instance_dir)/build/bin/, $(BIN_LINKS))

LIB_LINKS := libocclum-pal.so.$(major_ver) libocclum-pal.so
LIB_LINKS := $(addprefix $(instance_dir)/build/lib/, $(LIB_LINKS))

ifneq (, $(wildcard $(IMAGE)/. ))
	IMAGE_DIRS := $(shell find $(IMAGE) -type d 2>/dev/null | sed 's/ /\\ /g' || true)
	IMAGE_FILES := $(shell find $(IMAGE) -type f 2>/dev/null | sed 's/ /\\ /g' || true)
endif

SHELL:=/bin/bash

define get_conf_root_fs_mac
	LD_LIBRARY_PATH="$(SGX_SDK)/sdk_libs" \
		"$(occlum_dir)/build/bin/occlum-protect-integrity" show-mac "$(instance_dir)/build/mount/__ROOT/metadata"
endef

define get_occlum_conf_file_mac
	LD_LIBRARY_PATH="$(SGX_SDK)/sdk_libs" \
		"$(occlum_dir)/build/bin/occlum-protect-integrity" show-mac "$(instance_dir)/build/Occlum.json.protected"
endef

.PHONY : all clean

ALL_TARGETS := $(SIGNED_ENCLAVE) $(BIN_LINKS) $(LIB_LINKS)

all: $(ALL_TARGETS)

$(SIGNED_ENCLAVE): $(LIBOS)
	@echo "Signing the enclave..."

	@$(ENCLAVE_SIGN_TOOL) sign \
		-key $(ENCLAVE_SIGN_KEY) \
		-config "$(instance_dir)/build/Enclave.xml" \
		-enclave "$(instance_dir)/build/lib/libocclum-libos.so.$(major_ver)" \
		-out "$(instance_dir)/build/lib/libocclum-libos.signed.so"

$(LIBOS): $(instance_dir)/build/Occlum.json.protected
	@echo "Building libOS..."
	@export OCCLUM_BUILTIN_CONF_FILE_MAC=`$(get_occlum_conf_file_mac)` ; \
		echo "EXPORT => OCCLUM_BUILTIN_CONF_FILE_MAC = $$OCCLUM_BUILTIN_CONF_FILE_MAC" ; \
		cd $(instance_dir)/build/lib && \
		cp "$(occlum_dir)/build/lib/$(libos_lib).$(occlum_version)" . && ln -sf "$(libos_lib).$(occlum_version)" "libocclum-libos.so.$(major_ver)" && \
		ln -sf "libocclum-libos.so.$(major_ver)" libocclum-libos.so ; \
		echo -e "$$OCCLUM_BUILTIN_CONF_FILE_MAC\c" > temp_mac_file && \
		objcopy --update-section .builtin_config=temp_mac_file libocclum-libos.so && \
		rm temp_mac_file

$(instance_dir)/build/Occlum.json.protected: $(instance_dir)/build/Occlum.json
	@cd "$(instance_dir)/build" ; \
		LD_LIBRARY_PATH="$(SGX_SDK)/sdk_libs" "$(occlum_dir)/build/bin/occlum-protect-integrity" protect Occlum.json ;

$(instance_dir)/build/Enclave.xml:
$(instance_dir)/build/Occlum.json: $(SECURE_IMAGE) $(JSON_CONF) | $(instance_dir)/build/lib
	@$(occlum_dir)/build/bin/gen_internal_conf --user_json "$(instance_dir)/Occlum.json" --fs_mac `$(get_conf_root_fs_mac)` \
		--sdk_xml "$(instance_dir)/build/Enclave.xml" --sys_json $(instance_dir)/build/Occlum.json

$(BIN_LINKS): $(instance_dir)/build/bin/%: $(occlum_dir)/build/bin/% | $(instance_dir)/build/bin
	@ln -sf $< $@

$(instance_dir)/build/bin:
	@mkdir -p build/bin

$(instance_dir)/build/lib/libocclum-pal.so:
$(instance_dir)/build/lib/libocclum-pal.so.0: | $(instance_dir)/build/lib
	@cp "$(occlum_dir)/build/lib/$(pal_lib).$(occlum_version)" build/lib/
	@cd build/lib && ln -sf "$(pal_lib).$(occlum_version)" "libocclum-pal.so.$(major_ver)" && \
		ln -sf "libocclum-pal.so.$(major_ver)" libocclum-pal.so

$(instance_dir)/build/lib:
	@mkdir -p build/lib

# If image dir not exist, just use the secure Occlum FS image
ifneq ($(wildcard $(IMAGE)/. ),)
$(SECURE_IMAGE): $(IMAGE) $(IMAGE_DIRS) $(IMAGE_FILES) $(SEFS_CLI_SIM) $(SIGNED_SEFS_CLI_LIB)
	@echo "Building new image..."

	@rm -rf build/mount

	@mkdir -p build/mount/
	@LD_LIBRARY_PATH="$(SGX_SDK)/sdk_libs" $(SEFS_CLI_SIM) \
		--enclave "$(SIGNED_SEFS_CLI_LIB)" \
		zip \
		"$(instance_dir)/image" \
		"$(instance_dir)/build/mount/__ROOT" \
		--integrity-only
endif

clean:
	rm -rf $(instance_dir)/build
