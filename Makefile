.PHONY: test check install

test:
	PYTHONPATH=src:. python3 -m unittest discover -s tests -v

check: test
	python3 -m compileall -q src tools

install:
	python3 tools/install.py
