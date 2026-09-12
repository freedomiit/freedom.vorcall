using Vorcall.Server.Chat;

namespace Vorcall.Server.Tests;

// The grammars of PROTOCOL.md § Limits: names 1..32 scalars, topics and descriptions 0..256, a
// role icon emoji at most 2 scalars, no control characters anywhere.
public class NamesTests
{
    [Fact]
    public void a_name_keeps_its_casing_and_loses_its_surrounding_whitespace()
    {
        Assert.True(Names.TryNormalize("  General Chat\t", out var name));
        Assert.Equal("General Chat", name);
    }

    [Theory]
    [InlineData("")]
    [InlineData("   ")]
    public void a_name_that_trims_to_nothing_is_rejected(string raw)
    {
        Assert.False(Names.TryNormalize(raw, out var name));
        Assert.Equal(string.Empty, name);
    }

    [Fact]
    public void a_null_name_is_rejected()
    {
        Assert.False(Names.TryNormalize(null, out _));
    }

    [Theory]
    [InlineData(7)]
    [InlineData(9)]
    [InlineData(10)]
    public void a_name_with_a_control_character_is_rejected(int code)
    {
        // Bell, tab, line feed — inside the name, since a trailing tab would only be trimmed off.
        Assert.False(Names.TryNormalize("na" + (char)code + "me", out _));
    }

    [Fact]
    public void a_name_of_thirty_three_scalars_is_rejected()
    {
        Assert.False(Names.TryNormalize(new string('a', 33), out _));
    }

    [Fact]
    public void a_name_of_thirty_two_scalars_is_accepted_even_outside_the_basic_plane()
    {
        // 32 scalars but 64 UTF-16 code units: the limit counts scalars, not chars.
        var raw = string.Concat(Enumerable.Repeat("\U0001F642", 32));

        Assert.True(Names.TryNormalize(raw, out var name));
        Assert.Equal(raw, name);
    }

    [Fact]
    public void a_long_text_may_be_empty()
    {
        Assert.True(Names.TryNormalizeLong("   ", out var text));
        Assert.Equal(string.Empty, text);
    }

    [Fact]
    public void a_long_text_of_two_hundred_and_fifty_six_scalars_is_accepted()
    {
        var raw = new string('x', 256);

        Assert.True(Names.TryNormalizeLong(raw, out var text));
        Assert.Equal(raw, text);
    }

    [Fact]
    public void a_long_text_of_two_hundred_and_fifty_seven_scalars_is_rejected()
    {
        Assert.False(Names.TryNormalizeLong(new string('x', 257), out _));
    }

    [Fact]
    public void a_long_text_with_a_control_character_is_rejected()
    {
        Assert.False(Names.TryNormalizeLong("two\nlines", out _));
    }

    [Fact]
    public void an_empty_emoji_means_no_icon()
    {
        Assert.True(Names.TryNormalizeEmoji(string.Empty, out var emoji));
        Assert.Equal(string.Empty, emoji);
    }

    [Fact]
    public void an_emoji_may_be_a_base_with_a_variation_selector()
    {
        // U+1F6E1 plus the variation selector U+FE0F: two scalars, seven UTF-8 bytes.
        var shield = "\U0001F6E1" + (char)0xFE0F;

        Assert.True(Names.TryNormalizeEmoji(shield, out var emoji));
        Assert.Equal(shield, emoji);
    }

    [Fact]
    public void an_emoji_of_three_scalars_is_rejected()
    {
        Assert.False(Names.TryNormalizeEmoji("\U0001F642\U0001F642\U0001F642", out _));
    }

    [Fact]
    public void an_emoji_with_a_control_character_is_rejected()
    {
        Assert.False(Names.TryNormalizeEmoji("\U0001F642" + (char)1, out _));
    }
}
