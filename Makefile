PREFIX ?= /usr/local

BUILD_ROOT = build
SYSROOT = ./sysroot

ifdef RELEASE
	BUILD_DIR = $(BUILD_ROOT)/release
	CARGO_FLAGS = --release
	CARGO_TARGET_DIR = target/release
	CFLAGS += -O3
	ALUMINA_FLAGS += --sysroot $(SYSROOT)
else ifdef FAST_DEBUG
	# Compile in debug mode, but with alumina-boot compiled in release mode.
	# It is significantly faster.
	BUILD_DIR = $(BUILD_ROOT)/fast-debug
	CARGO_FLAGS = --release
	CARGO_TARGET_DIR = target/release
	CFLAGS += -g0
	ALUMINA_FLAGS += --sysroot $(SYSROOT) --debug
else ifdef COVERAGE
	CC = clang
	BUILD_DIR = $(BUILD_ROOT)/coverage
	CARGO_FLAGS = --profile coverage
	CARGO_TARGET_DIR = target/coverage
	CFLAGS += -g3 -fPIE -rdynamic -fprofile-instr-generate -fcoverage-mapping
	ALUMINA_FLAGS += --sysroot $(SYSROOT) --debug
	export RUSTFLAGS += -Cinstrument-coverage
	export LLVM_PROFILE_FILE = $(BUILD_ROOT)/coverage/profiles/%p-%m.profraw
else
	BUILD_DIR = $(BUILD_ROOT)/debug
	CARGO_FLAGS =
	CARGO_TARGET_DIR = target/debug
	CFLAGS += -g3 -fPIE -rdynamic
	ALUMINA_FLAGS += --sysroot $(SYSROOT) --debug
endif

LDFLAGS ?= -lm
ifndef STD_BACKTRACE
	ALUMINA_FLAGS += --cfg use_libbacktrace
	LDFLAGS += -lbacktrace
endif
ifndef NO_THREADS
	ALUMINA_FLAGS += --cfg threading
	LDFLAGS += -lpthread
endif

ifdef TIMINGS
	ALUMINA_FLAGS += --timings
endif


ALUMINA_BOOT = $(BUILD_DIR)/alumina-boot
ALUMINAC = $(BUILD_DIR)/aluminac
ALUMINAC_TESTS = $(BUILD_DIR)/aluminac-tests
CODEGEN = $(BUILD_DIR)/aluminac-generate
STDLIB_TESTS = $(BUILD_DIR)/stdlib-tests
LANG_TESTS = $(BUILD_DIR)/lang-tests
DOCTEST = $(BUILD_DIR)/doctest

# If grammar changes, we need to rebuild the world
COMMON_SOURCES = common/grammar.js
BOOTSTRAP_SOURCES = $(shell find src/alumina-boot/ -type f)
SYSROOT_FILES = $(shell find $(SYSROOT) -type f -name '*.alu')
ALU_LIBRARIES = $(shell find libraries/ -type f -name '*.alu')

SELFHOSTED_SOURCES = $(shell find src/aluminac/ -type f -name '*.alu')
CODEGEN_SOURCES = $(shell find tools/tree-sitter-codegen/ -type f -name '*.alu')

ALU_DEPS = $(ALUMINA_BOOT) $(SYSROOT_FILES) $(ALU_LIBRARIES)

# Ensure build directory exists, but do not pollute all the rules with it
$(BUILD_DIR)/.build:
	mkdir -p $(BUILD_DIR)
	touch $@

## ----------------- Bootstrap compiler (alumina-boot) -----------------

# alumina-boot is entirely built by cargo, it is here in the Makefile just so it can
# be a dependency and gets rebuilt if sources change.
$(ALUMINA_BOOT): $(BOOTSTRAP_SOURCES) $(COMMON_SOURCES) $(BUILD_DIR)/.build
	cargo build $(CARGO_FLAGS)
	cp $(CARGO_TARGET_DIR)/alumina-boot $(ALUMINA_BOOT)

## --------------------------- Stdlib tests ----------------------------

# Stdlib tests
$(STDLIB_TESTS).c: $(ALUMINA_BOOT) $(SYSROOT_FILES)
	$(ALUMINA_BOOT) $(ALUMINA_FLAGS) -Zdeny-warnings --cfg test --cfg test_std --output $@

$(STDLIB_TESTS): $(STDLIB_TESTS).c
	$(CC) $(CFLAGS) -o $@ $^ $(LDFLAGS)

## ---------------------------- Lang tests -----------------------------

LANG_TEST_FILES = $(shell find src/tests -type f -name '*.alu')

$(LANG_TESTS).c: $(ALUMINA_BOOT) $(SYSROOT_FILES) $(LANG_TEST_FILES)
	$(ALUMINA_BOOT) $(ALUMINA_FLAGS) -Zdeny-warnings --cfg test --output $@ \
		$(foreach src,$(LANG_TEST_FILES),$(subst /,::,$(basename $(subst src/tests/,lang_tests/,$(src))))=$(src)) \

$(LANG_TESTS): $(LANG_TESTS).c
	$(CC) $(CFLAGS) -o $@ $^ $(LDFLAGS)

## ------------------ Self-hosted compiler (aluminac) ------------------

# Compile tree sitter grammar to C. Bootstrap compiler does it by itself in the Cargo
# build script, but for aluminac, we need to do it in the Makefile.
$(BUILD_DIR)/src/parser.c: common/grammar.js
	cd $(BUILD_DIR) && tree-sitter generate --no-bindings $(abspath common/grammar.js)

$(BUILD_DIR)/parser.o: $(BUILD_DIR)/src/parser.c
	$(CC) $(CFLAGS) -I $(BUILD_DIR)/src -c $(BUILD_DIR)/src/parser.c -o $@ $(LDFLAGS)

# Codegen util for aluminac
$(CODEGEN).c: $(ALU_DEPS) $(CODEGEN_SOURCES)
	$(ALUMINA_BOOT) $(ALUMINA_FLAGS) --output $@ \
		$(foreach src,$(ALU_LIBRARIES),$(subst /,::,$(basename $(subst libraries/,,$(src))))=$(src)) \
		$(foreach src,$(CODEGEN_SOURCES),$(subst /,::,$(basename $(src)))=$(src))

$(CODEGEN): $(CODEGEN).c $(BUILD_DIR)/parser.o
	$(CC) $(CFLAGS) -o $@ $^ $(LDFLAGS) -ltree-sitter

src/aluminac/lib/node_kinds.alu: $(CODEGEN)
	$(CODEGEN) --output $@

# The actual self-hosted compiler
$(ALUMINAC).c: $(ALU_DEPS) $(SELFHOSTED_SOURCES) src/aluminac/lib/node_kinds.alu
	$(ALUMINA_BOOT) $(ALUMINA_FLAGS) --output $@ \
		$(foreach src,$(ALU_LIBRARIES),$(subst /,::,$(basename $(subst libraries/,,$(src))))=$(src)) \
		$(foreach src,$(SELFHOSTED_SOURCES),$(subst /,::,$(basename $(src)))=$(src))

$(ALUMINAC_TESTS).c: $(ALU_DEPS) $(SELFHOSTED_SOURCES) src/aluminac/lib/node_kinds.alu
	$(ALUMINA_BOOT) $(ALUMINA_FLAGS) --cfg test --output $@ \
		$(foreach src,$(ALU_LIBRARIES),$(subst /,::,$(basename $(subst libraries/,,$(src))))=$(src)) \
		$(foreach src,$(SELFHOSTED_SOURCES),$(subst /,::,$(basename $(src)))=$(src))

$(ALUMINAC): $(ALUMINAC).c $(BUILD_DIR)/parser.o
	$(CC) $(CFLAGS) -o $@ $^ $(LDFLAGS) -ltree-sitter

$(ALUMINAC_TESTS): $(ALUMINAC_TESTS).c $(BUILD_DIR)/parser.o
	$(CC) $(CFLAGS) -o $@ $^ $(LDFLAGS) -ltree-sitter

## --------------------------------Tools -------------------------------

ALUMINA_DOC = $(BUILD_DIR)/alumina-doc
ALUMINAC_LIB_SOURCES = $(shell find src/aluminac/lib/ -type f -name '*.alu')
ALUMINA_DOC_SOURCES = $(shell find tools/alumina-doc/ -type f -name '*.alu')

$(ALUMINA_DOC).c: $(ALU_DEPS) $(ALUMINAC_LIB_SOURCES) $(ALUMINA_DOC_SOURCES) src/aluminac/lib/node_kinds.alu
	$(ALUMINA_BOOT) $(ALUMINA_FLAGS) --output $@ \
		$(foreach src,$(ALU_LIBRARIES),$(subst /,::,$(basename $(subst libraries/,,$(src))))=$(src)) \
		$(foreach src,$(ALUMINAC_LIB_SOURCES),$(subst /,::,$(basename $(subst src/,,$(src))))=$(src)) \
		$(foreach src,$(ALUMINA_DOC_SOURCES),$(subst /,::,$(basename $(subst alumina-doc/,alumina_doc/,$(src))))=$(src))

$(ALUMINA_DOC): $(ALUMINA_DOC).c $(BUILD_DIR)/parser.o
	$(CC) $(CFLAGS) -o $@ $^ $(LDFLAGS) -ltree-sitter

$(BUILD_DIR)/doctest.alu: $(ALUMINA_DOC) $(SYSROOT_FILES) tools/alumina-doc/static/*
	@mkdir -p $(BUILD_DIR)/~doctest
	ALUMINADOC_OUTPUT_DIR=$(BUILD_DIR)/~doctest $(ALUMINA_DOC) \
		$(foreach src,$(SYSROOT_FILES),$(subst __root__,, $(subst /,::,$(basename $(subst ./sysroot,,$(src)))))=$(src))
	@cp -rf tools/alumina-doc/static $(BUILD_DIR)/~doctest/html/
	@rm -rf $(BUILD_DIR)/html $(BUILD_DIR)/doctest.alu
	mv $(BUILD_DIR)/~doctest/* $(BUILD_DIR)/
	@rmdir $(BUILD_DIR)/~doctest

$(DOCTEST).c: $(ALUMINA_BOOT) $(SYSROOT_FILES) $(BUILD_DIR)/doctest.alu
	$(ALUMINA_BOOT) $(ALUMINA_FLAGS) --cfg test --output $@ $(BUILD_DIR)/doctest.alu

$(DOCTEST): $(DOCTEST).c
	$(CC) $(CFLAGS) -o $@ $^ $(LDFLAGS)

.PHONY: docs test-docs serve-docs watch-docs
docs: $(BUILD_DIR)/doctest.alu

test-docs: $(DOCTEST)
	$(DOCTEST) $(TEST_FLAGS) || true

serve-docs:
	@cd $(BUILD_DIR)/html && python3 -m http.server

watch-docs:
	@BUILD_DIR=$(BUILD_DIR) tools/alumina-doc/watch_docs.sh

## ------------------------------ Examples -----------------------------

.PHONY: examples examples

EXAMPLES = $(shell find examples/ -type f -name '*.alu')

$(BUILD_DIR)/examples/.build:
	mkdir -p $(BUILD_DIR)/examples
	touch $@

$(BUILD_DIR)/examples/%: examples/%.alu $(ALUMINA_BOOT) $(SYSROOT_FILES) $(BUILD_DIR)/examples/.build
	$(ALUMINA_BOOT) $(ALUMINA_FLAGS) --output $@.c main=$< && \
	$(CC) $(CFLAGS) -o $@ $@.c $(LDFLAGS)

examples: $(patsubst examples/%.alu,$(BUILD_DIR)/examples/%,$(EXAMPLES))

## ------------------------------ Various ------------------------------

.PHONY: clean all install
clean:
	cargo clean
	rm -rf $(BUILD_ROOT)/
	rm -f quick.c quick alumina-boot aluminac

install: $(ALUMINA_BOOT) $(SYSROOT_FILES)
	install -T $(ALUMINA_BOOT) $(PREFIX)/bin/alumina-boot
	rm -rf $(PREFIX)/share/alumina
	mkdir -p $(PREFIX)/share/alumina
	cp -r $(SYSROOT)/* $(PREFIX)/share/alumina

# Some convenience symlinks
alumina-boot: $(ALUMINA_BOOT)
	ln -sf $(ALUMINA_BOOT) $@

aluminac: $(ALUMINAC)
	ln -sf $(ALUMINAC) $@

.PHONY: test-std test-examples test-alumina-boot test-aluminac test-lang test

test-std: alumina-boot $(STDLIB_TESTS)
	$(STDLIB_TESTS) $(TEST_FLAGS)

test-lang: alumina-boot $(LANG_TESTS)
	$(LANG_TESTS) $(TEST_FLAGS)

test-alumina-boot:
	cargo test $(CARGO_FLAGS) --all-targets

test-aluminac: $(ALUMINAC_TESTS)
	$(ALUMINAC_TESTS) $(TEST_FLAGS)

test: test-alumina-boot test-std test-lang

.DEFAULT_GOAL := all
all: alumina-boot aluminac

## ------------------ Ad-hoc manual testing shortcuts ------------------

$(BUILD_DIR)/quick.c: $(ALUMINA_BOOT) $(SYSROOT_FILES) quick.alu
	$(ALUMINA_BOOT) $(ALUMINA_FLAGS) --output $@ quick=./quick.alu

$(BUILD_DIR)/quick: $(BUILD_DIR)/quick.c
	$(CC) $(CFLAGS) -o $@ $^ $(LDFLAGS)

quick: $(BUILD_DIR)/quick
	ln -sf $^.c $@.c
	ln -sf $^ $@

## ------------------------------ Benchmarking -------------------------

.PHONY: bench-std bench-std-cc

BENCH_CMD = ./tools/bench.py -n$(if $(TIMES),$(TIMES),20) $(if $(MARKDOWN),--markdown,)

bench-std: $(ALUMINA_BOOT) $(SYSROOT_FILES)
	$(BENCH_CMD) $(ALUMINA_BOOT) $(ALUMINA_FLAGS) --timings --cfg test --cfg test_std --output /dev/null

bench-std-cc: $(STDLIB_TESTS).c
	$(BENCH_CMD) $(CC) $(CFLAGS) -o/dev/null $^ $(LDFLAGS)

## ------------------------------ Coverage ------------------------------
.PHONY: coverage dist-check-with-coverage
coverage:
	COVERAGE=1 $(MAKE) dist-check-with-coverage

dist-check-with-coverage: dist-check
	llvm-profdata merge \
		-sparse  \
		$(BUILD_DIR)/profiles/* \
		-o $(BUILD_DIR)/profiles/merged.profdata

	llvm-cov export \
		-Xdemangler=rustfilt \
		-format=lcov \
		-instr-profile=$(BUILD_DIR)/profiles/merged.profdata $(ALUMINA_BOOT) \
		$(BOOTSTRAP_SOURCES) > $(BUILD_DIR)/coverage.txt

	llvm-cov show \
		-Xdemangler=rustfilt \
		-format=html \
		-instr-profile=$(BUILD_DIR)/profiles/merged.profdata $(ALUMINA_BOOT) \
		-output-dir=$(BUILD_DIR)/html \
		$(BOOTSTRAP_SOURCES)

serve-coverage:
	@cd $(BUILD_ROOT)/coverage/html && python3 -m http.server

## ------------------------------ Dist ----------------------------------

.PHONY: lint-rust dist-check

lint-rust: $(BOOTSTRAP_SOURCES) $(COMMON_SOURCES) $(BUILD_DIR)/.build
	cargo fmt -- --check
	cargo clippy $(CARGO_FLAGS) --all-targets

dist-check: lint-rust aluminac test-docs test examples
