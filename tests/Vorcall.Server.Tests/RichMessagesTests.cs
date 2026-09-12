using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class RichMessagesTests(ServerFixture fixture)
{
    private const string Fire = "🔥";

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task EditMessage_belongs_to_the_author_and_MessageEdited_reaches_the_channel()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, b1, c1) = (party.Account(0), party.Client(0), party.Client(1), party.Client(2));
        var general = party.GeneralId;
        var token = Names.Token();

        await a1.SendAsync(Frames.Send($"edit me {token}", general));
        var targetId = await party.ExpectMessageEverywhereAsync($"edit me {token}", general);

        var editedText = $"edited {token}";
        await a1.SendAsync(Frames.Edit(targetId, editedText));
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.MessageEdited))
        {
            var edited = frame.MessageEdited.Message;
            Assert.Equal(targetId, edited.Id);
            Assert.Equal(general, edited.ChannelId);
            Assert.Equal(editedText, edited.Text);
            Assert.True(edited.EditedAtUnixMs > 0);
        }

        // MANAGE_MESSAGES does not grant editing someone else's text, so this stays FORBIDDEN rather
        // than naming a bit the caller could be given.
        await b1.SendAsync(Frames.Edit(targetId, "not mine"));
        await b1.ExpectErrorAsync(ErrorCode.Forbidden, fatal: false);

        await a1.SendAsync(Frames.Edit(targetId + 1_000_000, "nobody's"));
        await a1.ExpectErrorAsync(ErrorCode.UnknownMessage, fatal: false);

        var stored = (await History.ByIdAsync(Server, alice.Access, general))[targetId];
        Assert.Equal(editedText, stored.Text);
        Assert.True(stored.EditedAtUnixMs > 0);
        await c1.QuietAsync();
    }

    [Fact]
    public async Task DeleteMessage_leaves_a_tombstone_nothing_can_touch()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, b1, c1) = (party.Account(0), party.Client(0), party.Client(1), party.Client(2));
        var general = party.GeneralId;
        var token = Names.Token();

        await a1.SendAsync(Frames.Send($"keep me {token}", general));
        var keptId = await party.ExpectMessageEverywhereAsync($"keep me {token}", general);
        await a1.SendAsync(Frames.Send($"delete me {token}", general));
        var doomedId = await party.ExpectMessageEverywhereAsync($"delete me {token}", general);

        await a1.SendAsync(Frames.Delete(doomedId));
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.MessageDeleted))
        {
            Assert.Equal(general, frame.MessageDeleted.ChannelId);
            Assert.Equal(doomedId, frame.MessageDeleted.Id);
        }

        var stored = (await History.ByIdAsync(Server, alice.Access, general))[doomedId];
        Assert.True(stored.Deleted);
        Assert.Equal(string.Empty, stored.Text);
        Assert.Empty(stored.Reactions);
        Assert.Empty(stored.Attachments);

        foreach (var frame in new[] { Frames.Edit(doomedId, "back from the dead"), Frames.Delete(doomedId) })
        {
            await a1.SendAsync(frame);
            await a1.ExpectErrorAsync(ErrorCode.UnknownMessage, fatal: false);
        }

        // Someone else's message is not refused outright: it names the bit that would let it go.
        await b1.SendAsync(Frames.Delete(keptId));
        Assert.Equal(
            PermNames.Name(Perm.ManageMessages),
            (await b1.ExpectErrorAsync(ErrorCode.PermissionDenied, fatal: false)).Detail);
        await c1.QuietAsync();
    }

    [Fact]
    public async Task React_groups_by_emoji_with_ascending_user_ids()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1) = (party.Account(0), party.Client(0));
        var (bob, b1) = (party.Account(1), party.Client(1));
        var general = party.GeneralId;
        var token = Names.Token();

        await a1.SendAsync(Frames.Send($"react to me {token}", general));
        var targetId = await party.ExpectMessageEverywhereAsync($"react to me {token}", general);
        await a1.SendAsync(Frames.Send($"gone {token}", general));
        var doomedId = await party.ExpectMessageEverywhereAsync($"gone {token}", general);
        await a1.SendAsync(Frames.Delete(doomedId));
        await party.ExpectEverywhereAsync(Kind.MessageDeleted);

        var reactors = new[] { alice.UserId, bob.UserId }.Order().ToArray();
        var steps = new (WsClient Reactor, bool Remove, (string Emoji, long[] UserIds)[] Expected)[]
        {
            (a1, false, [(Fire, [alice.UserId])]),
            (b1, false, [(Fire, reactors)]),
            (b1, true, [(Fire, [alice.UserId])]),
            (a1, true, []),
        };
        foreach (var (reactor, remove, expected) in steps)
        {
            await reactor.SendAsync(Frames.React(targetId, Fire, remove));
            foreach (var frame in await party.ExpectEverywhereAsync(Kind.ReactionsChanged))
            {
                var changed = frame.ReactionsChanged;
                Assert.Equal(general, changed.ChannelId);
                Assert.Equal(targetId, changed.MessageId);
                Assert.Equal(Render(expected), Render(changed.Reactions.Select(group => (group.Emoji, group.UserIds.ToArray()))));
            }
        }

        await a1.SendAsync(Frames.React(targetId, "💩"));
        await a1.ExpectErrorAsync(ErrorCode.InvalidReaction, fatal: false);
        foreach (var dead in new[] { doomedId, targetId + 1_000_000 })
        {
            await a1.SendAsync(Frames.React(dead, Fire));
            await a1.ExpectErrorAsync(ErrorCode.UnknownMessage, fatal: false);
        }

        await party.Client(2).QuietAsync();
    }

    [Fact]
    public async Task A_reply_carries_an_excerpt_of_its_targets_current_text()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, b1, c1) = (party.Account(0), party.Client(0), party.Client(1), party.Client(2));
        var general = party.GeneralId;
        var token = Names.Token();

        // A message of another channel, for the cross-channel refusal below.
        var dmId = await party.OpenDmAsync(0, 1);
        await a1.SendAsync(Frames.Send($"dm only {token}", dmId));
        long dmMessageId = 0;
        foreach (var client in new[] { a1, b1 })
        {
            dmMessageId = (await client.ExpectAsync(Kind.Message)).Message.Id;
        }

        var longText = $"reply target {token} " + new string('z', 200);
        await a1.SendAsync(Frames.Send(longText, general));
        var repliedId = await party.ExpectMessageEverywhereAsync(longText, general);

        var replyText = $"replying {token}";
        await b1.SendAsync(Frames.Send(replyText, general, replyToId: repliedId));
        long replyId = 0;
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.Message))
        {
            var message = frame.Message;
            Assert.Equal(replyText, message.Text);
            Assert.Equal(repliedId, message.ReplyTo.Id);
            Assert.Equal(alice.Username, message.ReplyTo.Author);
            Assert.Equal(longText[..120], message.ReplyTo.Excerpt);
            Assert.False(message.ReplyTo.Deleted);
            replyId = message.Id;
        }

        var stored = (await History.ByIdAsync(Server, alice.Access, general))[replyId];
        Assert.Equal(repliedId, stored.ReplyTo.Id);
        Assert.Equal(longText[..120], stored.ReplyTo.Excerpt);

        foreach (var badTarget in new[] { repliedId + 1_000_000, dmMessageId })
        {
            await a1.SendAsync(Frames.Send("into the void", general, replyToId: badTarget));
            await a1.ExpectErrorAsync(ErrorCode.UnknownMessage, fatal: false);
        }

        await c1.QuietAsync();
    }

    private static string Render(IEnumerable<(string Emoji, long[] UserIds)> groups)
        => string.Join(" ", groups.Select(group => $"{group.Emoji}:{string.Join(",", group.UserIds)}"));
}
