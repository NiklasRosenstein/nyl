from nyl.packages.reference import Reference


def test_sub_references() -> None:
    ref = Reference("Secret", None, "my-secret", "World")
    value = f"Hello {ref}!"

    def repl(ref: Reference) -> str:
        return ref.key

    assert Reference.sub([value], repl) == ["Hello World!"]
    assert list(Reference.collect([value])) == [ref]
