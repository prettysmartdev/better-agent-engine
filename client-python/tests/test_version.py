import re

import base_client


def test_version_is_semver():
    assert re.fullmatch(r"\d+\.\d+\.\d+", base_client.__version__)
