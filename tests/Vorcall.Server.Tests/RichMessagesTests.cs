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
    public async Task EditMessage_belongs_to_the_author_and_MessageEdited_reaches_the_room()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, b1, c1) = (party.Account(0), party.Client(0), party.Client(1), party.Client(2));
        var token = Names.Token();

        await a1.SendAsync(Frames.Send($"edit me {token}", Session.General));
        var targetId = await party.ExpectMessageEverywhereAsync($"edit me {token}");

        var editedText = $"edited {token}";
        await a1.SendAsync(Frames.Edit(targetId, editedText));
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.MessageEdited))
        {
            var edited = frame.MessageEdited.Message;
            Assert.Equal(targetId, edited.Id);
            Assert.Equal(editedText, edited.Text);
            Assert.True(edited.EditedAtUnixMs > 0);
        }

        await b1.SendAsync(Frames.Edit(targetId, "not mine"));
        await b1.ExpectErrorAsync(ErrorCode.Forbidden, fatal: false);

        await a1.SendAsync(Frames.Edit(targetId + 1_000_000, "nobody's"));
        await a1.ExpectErrorAsync(ErrorCode.UnknownMessage, fatal: false);

        var stored = (await History.ByIdAsync(Server, alice.Access))[targetId];
        Assert.Equal(editedText, stored.Text);
        Assert.True(stored.EditedAtUnixMs > 0);
        await c1.QuietAsync();
    }

    [Fact]
    public async Task DeleteMessage_leaves_a_tombstone_nothing_can_touch()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, b1, c1) = (party.Account(0), party.Client(0), party.Client(1), party.Client(2));
        var token = Names.Token();

        await a1.SendAsync(Frames.Send($"keep me {token}", Session.General));
        var keptId = await party.ExpectMessageEverywhereAsync($"keep me {token}");
        await a1.SendAsync(Frames.Send($"delete me {token}", Session.General));
        var doomedId = await party.ExpectMessageEverywhereAsync($"delete me {token}");

        await a1.SendAsync(Frames.Delete(doomedId));
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.MessageDeleted))
        {
            Assert.Equal(Session.General, frame.MessageDeleted.RoomId);
            Assert.Equal(doomedId, frame.MessageDeleted.Id);
        }

        var stored = (await History.ByIdAsync(Server, alice.Access))[doomedId];
        Assert.True(stored.Deleted);
        Assert.Equal(string.Empty, stored.Text);
        Assert.Empty(stored.Reactions);
        Assert.Empty(stored.Attachments);

        foreach (var frame in new[] { Frames.Edit(doomedId, "back from the dead"), Frames.Delete(doomedId) })
        {
            await a1.SendAsync(frame);
            await a1.ExpectErrorAsync(ErrorCode.UnknownMessage, fatal: false);
        }

        await b1.SendAsync(Frames.Delete(keptId));
        await b1.ExpectErrorAsync(ErrorCode.Forbidden, fatal: false);
        await c1.QuietAsync();
    }

    [Fact]
    public async Task React_groups_by_emoji_with_ascending_user_ids()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1) = (party.Account(0), party.Client(0));
        var (bob, b1) = (party.Account(1), party.Client(1));
        var token = Names.Token();

        await a1.SendAsync(Frames.Send($"react to me {token}", Session.General));
        var targetId = await party.ExpectMessageEverywhereAsync($"react to me {token}");
        await a1.SendAsync(Frames.Send($"gone {token}", Session.General));
        var doomedId = await party.ExpectMessageEverywhereAsync($"gone {token}");
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
                Assert.Equal(Session.General, changed.RoomId);
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
        var token = Names.Token();

        // A message of another room, for the cross-room refusal below.
        var dmId = await party.OpenDmAsync(0, 1);
        await a1.SendAsync(Frames.Send($"dm only {token}", dmId));
        long dmMessageId = 0;
        foreach (var client in new[] { a1, b1 })
        {
            dmMessageId = (await client.ExpectAsync(Kind.Message)).Message.Id;
        }

        var longText = $"reply target {token} " + new string('z', 200);
        await a1.SendAsync(Frames.Send(longText, Session.General));
        var repliedId = await party.ExpectMessageEverywhereAsync(longText);

        var replyText = $"replying {token}";
        await b1.SendAsync(Frames.Send(replyText, Session.General, replyToId: repliedId));
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

        var stored = (await History.ByIdAsync(Server, alice.Access))[replyId];
        Assert.Equal(repliedId, stored.ReplyTo.Id);
        Assert.Equal(longText[..120], stored.ReplyTo.Excerpt);

        foreach (var badTarget in new[] { repliedId + 1_000_000, dmMessageId })
        {
            await a1.SendAsync(Frames.Send("into the void", Session.General, replyToId: badTarget));
            await a1.ExpectErrorAsync(ErrorCode.UnknownMessage, fatal: false);
        }

        await c1.QuietAsync();
    }

    private static string Render(IEnumerable<(string Emoji, long[] UserIds)> groups)
        => string.Join(" ", groups.Select(group => $"{group.Emoji}:{string.Join(",", group.UserIds)}"));
}
