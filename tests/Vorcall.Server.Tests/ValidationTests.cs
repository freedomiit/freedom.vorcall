using Vorcall.Server.Chat;

namespace Vorcall.Server.Tests;

// Channel ids are positive bigints — 0 is the wire's "absent" and names no channel — and
// @everyone / @here are literal words at a word boundary (PROTOCOL.md § Messages).
public class ValidationTests
{
    [Fact]
    public void the_channel_id_zero_names_no_channel()
    {
        Assert.False(Validation.TryParseChannelId(0L, out var id));
        Assert.Equal(0L, id);
    }

    [Fact]
    public void a_negative_channel_id_names_no_channel()
    {
        Assert.False(Validation.TryParseChannelId(-7L, out _));
    }

    [Fact]
    public void a_positive_channel_id_is_accepted()
    {
        Assert.True(Validation.TryParseChannelId(12L, out var id));
        Assert.Equal(12L, id);
    }

    [Fact]
    public void a_query_string_channel_id_is_parsed()
    {
        Assert.True(Validation.TryParseChannelId("12", out var id));
        Assert.Equal(12L, id);
    }

    [Theory]
    [InlineData("-1")]
    [InlineData("abc")]
    [InlineData("")]
    [InlineData(" 12")]
    [InlineData("12345678901234567890")]

    // 19 digits, so the length guard lets it through: the parse is what catches the overflow.
    [InlineData("9999999999999999999")]
    public void a_query_string_that_is_not_a_positive_id_is_rejected(string raw)
    {
        Assert.False(Validation.TryParseChannelId(raw, out var id));
        Assert.Equal(0L, id);
    }

    [Fact]
    public void everyone_is_flagged_after_whitespace()
    {
        var (everyone, here) = Validation.MentionFlags("hi @everyone");

        Assert.True(everyone);
        Assert.False(here);
    }

    [Fact]
    public void here_is_flagged_at_the_start_and_before_punctuation()
    {
        var (everyone, here) = Validation.MentionFlags("@here!");

        Assert.False(everyone);
        Assert.True(here);
    }

    [Fact]
    public void an_email_address_is_not_a_mention()
    {
        var (everyone, here) = Validation.MentionFlags("email@everyone.com");

        Assert.False(everyone);
        Assert.False(here);
    }

    [Fact]
    public void a_word_glued_to_here_is_not_a_mention()
    {
        var (_, here) = Validation.MentionFlags("x@here");

        Assert.False(here);
    }

    [Fact]
    public void the_match_is_case_sensitive()
    {
        var (everyone, _) = Validation.MentionFlags("@Everyone");

        Assert.False(everyone);
    }

    [Fact]
    public void a_trailing_comma_still_closes_the_word()
    {
        var (everyone, _) = Validation.MentionFlags("@everyone,");

        Assert.True(everyone);
    }

    [Fact]
    public void both_words_can_appear_in_one_message()
    {
        var (everyone, here) = Validation.MentionFlags("say @here and @everyone");

        Assert.True(everyone);
        Assert.True(here);
    }
}
