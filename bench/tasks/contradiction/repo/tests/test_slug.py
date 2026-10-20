from text.slug import slug


def test_slug_uses_hyphens():
    assert slug("Hello World") == "hello-world"


def test_slug_for_filenames_uses_underscores():
    assert slug("Hello World") == "hello_world"
