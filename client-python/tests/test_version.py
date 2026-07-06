import re

import bae_py


def test_version_is_semver():
    assert re.fullmatch(r"\d+\.\d+\.\d+", bae_py.__version__)
