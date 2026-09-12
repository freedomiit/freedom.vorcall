using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

// Channels and categories over the wire. There is no joining and no leaving: every account belongs
// to the server and a channel is visible exactly when VIEW_CHANNEL resolves for that member, so
// "who is in this channel" is "everyone who may view it" and what used to be a membership is a
// permission here. Every management frame needs MANAGE_CHANNELS, which on a fresh server only the
// owner holds, so most of these drive the owner party.
[Collection(ServerCollection.Name)]
public sealed class ChannelsTests(ServerFixture fixture)
{
    private const string Fire = "🔥";

    private const ulong View = (ulong)Perm.ViewChannel;

    // Far past any serial id this suite's database will reach, for the frames that have to name a
    // channel or a category that does not exist.
    private const long UnknownId = 9_000_000;

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task CreateChannel_keeps_the_name_verbatim_and_reaches_every_member_who_may_view_it()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "alice");
        var token = Names.Token();
        var name = $"Team  Chat_2 {token}";
        var topic = $"what we talk about {token}";
        var categoryId = party.Client(0).Session.GeneralCategoryId;

        await party.Client(0).SendAsync(Frames.CreateChannel(ChannelKind.Text, $"  {name}  ", topic, categoryId));

        long id = 0;
        foreach (var client in party.Clients)
        {
            var channel = (await client.ExpectAsync(Kind.ChannelUpserted)).ChannelUpserted.Channel;

            // Trimmed and nothing else: a channel name is not slugged, not lower-cased and not
            // rewritten into an id the client could have predicted.
            Assert.Equal(name, channel.Name);
            Assert.Equal(topic, channel.Topic);
            Assert.Equal(ChannelKind.Text, channel.Kind);
            Assert.Equal(categoryId, channel.CategoryId);
            Assert.Empty(channel.Overrides);
            Assert.Empty(channel.DmMemberIds);
            Assert.True(channel.Id != 0, $"{client.Label}: the new channel has no id");
            Assert.True(id == 0 || id == channel.Id, $"{client.Label}: channel id {channel.Id} differs from {id}");
            id = channel.Id;
        }

        // The same name again: channel names need not be unique, so this is a second channel with a
        // second id rather than a refusal.
        await party.Client(0).SendAsync(Frames.CreateChannel(ChannelKind.Text, name, categoryId: categoryId));
        foreach (var client in party.Clients)
        {
            var twin = (await client.ExpectAsync(Kind.ChannelUpserted)).ChannelUpserted.Channel;
            Assert.Equal(name, twin.Name);
            Assert.NotEqual(id, twin.Id);
        }

        // The creator is answered by that same broadcast and by nothing else: there is no separate
        // acknowledgement.
        foreach (var client in party.Clients)
        {
            await client.QuietAsync();
        }
    }

    [Fact]
    public async Task CreateChannel_refuses_an_unusable_name_a_long_topic_an_unknown_kind_and_an_unknown_category()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "bob");
        var owner = party.Client(0);

        foreach (var bad in new[] { string.Empty, "   ", new string('n', 33), "na\tme" })
        {
            await owner.SendAsync(Frames.CreateChannel(ChannelKind.Text, bad));
            await owner.ExpectErrorAsync(ErrorCode.InvalidName, fatal: false);
        }

        await owner.SendAsync(Frames.CreateChannel(ChannelKind.Text, $"topic {Names.Token()}", new string('t', 257)));
        Assert.Equal("topic", (await owner.ExpectErrorAsync(ErrorCode.InvalidArgument, fatal: false)).Detail);

        // A DM is opened with OpenDm, never created here.
        foreach (var kind in new[] { ChannelKind.Unspecified, ChannelKind.Dm })
        {
            await owner.SendAsync(Frames.CreateChannel(kind, $"kind {Names.Token()}"));
            Assert.Equal("kind", (await owner.ExpectErrorAsync(ErrorCode.InvalidArgument, fatal: false)).Detail);
        }

        await owner.SendAsync(Frames.CreateChannel(ChannelKind.Text, $"orphan {Names.Token()}", categoryId: UnknownId));
        await owner.ExpectErrorAsync(ErrorCode.UnknownCategory, fatal: false);

        await party.Client(1).QuietAsync();
    }

    [Fact]
    public async Task Managing_a_channel_needs_MANAGE_CHANNELS_and_a_granted_role_is_enough()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "carol");
        var member = party.Client(1);
        var (_, id) = await party.CreateChannelAsync();

        foreach (var frame in new[]
        {
            Frames.CreateChannel(ChannelKind.Text, $"mine {Names.Token()}"),
            Frames.UpdateChannel(id, $"renamed {Names.Token()}"),
            Frames.DeleteChannel(id),
            Frames.CreateCategory($"section {Names.Token()}"),
            Frames.SetOverride(id, Frames.RoleOverride(party.EveryoneId, allow: (ulong)Perm.MentionEveryone)),
        })
        {
            await member.SendAsync(frame);
            Assert.Equal(
                PermNames.Name(Perm.ManageChannels),
                (await member.ExpectErrorAsync(ErrorCode.PermissionDenied, fatal: false)).Detail);
        }

        await party.Client(0).QuietAsync();

        // An ordinary member holding exactly that bit, rather than the owner: the owner bypasses
        // every check, so it can never prove which bit a frame asks for.
        await party.GrantRoleAsync(1, (ulong)Perm.ManageChannels);

        var name = $"theirs {Names.Token()}";
        await member.SendAsync(Frames.CreateChannel(ChannelKind.Text, name));
        foreach (var client in party.Clients)
        {
            Assert.Equal(name, (await client.ExpectAsync(Kind.ChannelUpserted)).ChannelUpserted.Channel.Name);
        }
    }

    [Fact]
    public async Task UpdateChannel_and_DeleteChannel_reach_the_channels_viewers()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "dora");
        var owner = party.Client(0);
        var (_, id) = await party.CreateChannelAsync();

        var renamed = $"renamed {Names.Token()}";
        var topic = $"now with a topic {Names.Token()}";
        await owner.SendAsync(Frames.UpdateChannel(id, renamed, topic));
        foreach (var client in party.Clients)
        {
            var channel = await client.ExpectChannelUpsertedAsync(id);
            Assert.Equal(renamed, channel.Name);
            Assert.Equal(topic, channel.Topic);
        }

        // An empty topic clears it; an empty name is not a value at all.
        await owner.SendAsync(Frames.UpdateChannel(id, renamed));
        foreach (var client in party.Clients)
        {
            Assert.Equal(string.Empty, (await client.ExpectChannelUpsertedAsync(id)).Topic);
        }

        await owner.SendAsync(Frames.UpdateChannel(id, string.Empty));
        await owner.ExpectErrorAsync(ErrorCode.InvalidName, fatal: false);

        await owner.SendAsync(Frames.DeleteChannel(id));
        foreach (var client in party.Clients)
        {
            await client.ExpectChannelDeletedAsync(id);
        }

        // The id names nothing now, and nothing tells that apart from a channel the caller may not
        // view.
        await owner.SendAsync(Frames.UpdateChannel(id, renamed));
        await owner.ExpectErrorAsync(ErrorCode.UnknownChannel, fatal: false);
        await party.Client(1).QuietAsync();
    }

    [Fact]
    public async Task General_can_neither_be_deleted_nor_hidden_from_everyone()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "erin");
        var owner = party.Client(0);

        await owner.SendAsync(Frames.DeleteChannel(party.GeneralId));
        await owner.ExpectErrorAsync(ErrorCode.Forbidden, fatal: false);

        // Refused at write time before a row is touched, and resolution forces VIEW_CHANNEL on for
        // general anyway: PROTOCOL.md § Server, channels and categories.
        await owner.SendAsync(Frames.SetOverride(party.GeneralId, Frames.RoleOverride(party.EveryoneId, deny: View)));
        Assert.Equal("deny", (await owner.ExpectErrorAsync(ErrorCode.InvalidArgument, fatal: false)).Detail);

        Assert.Contains(party.GeneralId, party.Client(1).Session.Channels.Keys);
        foreach (var client in party.Clients)
        {
            await client.QuietAsync();
        }
    }

    [Fact]
    public async Task Visibility_is_a_permission_and_not_a_membership()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "fran", "gina");
        var (owner, hidden, other) = (party.Client(0), party.Client(1), party.Client(2));
        var hiddenId = party.Account(1).UserId;
        var (_, id) = await party.CreateChannelAsync();

        // Driven through Frames.SetOverride rather than Party.SetMemberOverrideAsync, which refuses a
        // VIEW_CHANNEL mask: these frames depend on who gained or lost sight of the channel.
        await owner.SendAsync(Frames.SetOverride(id, Frames.MemberOverride(hiddenId, deny: View)));

        // The visibility sweep runs before the upsert, so the member that lost the channel hears
        // ChannelDeleted and is no longer among the audience the upsert is measured against.
        await hidden.ExpectChannelDeletedAsync(id);
        foreach (var client in new[] { owner, other })
        {
            var over = Assert.Single((await client.ExpectChannelUpsertedAsync(id)).Overrides);
            Assert.Equal(hiddenId, over.UserId);
            Assert.Equal(View, over.Deny);
        }

        // An invisible channel is indistinguishable from one that never existed.
        await hidden.SendAsync(Frames.Send("still here?", id));
        await hidden.ExpectErrorAsync(ErrorCode.UnknownChannel, fatal: false);

        var text = $"without you {Names.Token()}";
        await other.SendAsync(Frames.Send(text, id));
        foreach (var client in new[] { owner, other })
        {
            Assert.Equal(text, (await client.ExpectAsync(Kind.Message)).Message.Text);
        }

        await hidden.QuietAsync();

        // Clearing the override hands the channel back. The restored member is upserted twice: once
        // by the sweep that gave it sight, once by the broadcast that now counts it as a viewer.
        await owner.SendAsync(Frames.SetOverride(id, Frames.MemberOverride(hiddenId)));
        for (var upsert = 0; upsert < 2; upsert++)
        {
            Assert.Empty((await hidden.ExpectChannelUpsertedAsync(id)).Overrides);
        }

        foreach (var client in new[] { owner, other })
        {
            Assert.Empty((await client.ExpectChannelUpsertedAsync(id)).Overrides);
        }

        var back = $"back again {Names.Token()}";
        await hidden.SendAsync(Frames.Send(back, id));
        await party.ExpectMessageEverywhereAsync(back, id);
    }

    [Fact]
    public async Task A_hidden_channel_stays_hidden_across_a_reconnect_and_visible_to_everyone_else()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "hana", "iris");
        var owner = party.Client(0);
        var (_, id) = await party.CreateChannelAsync();

        var text = $"before the curtain {Names.Token()}";
        await party.Client(1).SendAsync(Frames.Send(text, id));
        await party.ExpectMessageEverywhereAsync(text, id);

        await owner.SendAsync(Frames.SetOverride(id, Frames.MemberOverride(party.Account(1).UserId, deny: View)));
        await party.Client(1).ExpectChannelDeletedAsync(id);
        foreach (var index in new[] { 0, 2 })
        {
            await party.Client(index).ExpectChannelUpsertedAsync(id);
        }

        // The override is a row, so the next hello resolves the same answer: neither the channel nor
        // its read counters are in the snapshot of the member that may not view it.
        await party.ReconnectAsync(Server, 1);
        var hidden = party.Client(1).Session;
        Assert.DoesNotContain(id, hidden.Channels.Keys);
        Assert.DoesNotContain(id, hidden.ReadStates.Keys);
        Assert.Contains(party.GeneralId, hidden.Channels.Keys);

        await party.ReconnectAsync(Server, 2);
        var seen = party.Client(2).Session;
        Assert.Contains(id, seen.Channels.Keys);
        Assert.Contains(id, seen.ReadStates.Keys);

        await owner.QuietAsync();
    }

    [Fact]
    public async Task Editing_deleting_or_reacting_in_a_channel_the_caller_cannot_view_answers_UNKNOWN_CHANNEL()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "jade");
        var (owner, author) = (party.Client(0), party.Client(1));
        var (_, id) = await party.CreateChannelAsync();

        var text = $"mine for now {Names.Token()}";
        await author.SendAsync(Frames.Send(text, id));
        var messageId = await party.ExpectMessageEverywhereAsync(text, id);

        await owner.SendAsync(Frames.SetOverride(id, Frames.MemberOverride(party.Account(1).UserId, deny: View)));
        await author.ExpectChannelDeletedAsync(id);
        await owner.ExpectChannelUpsertedAsync(id);

        // Its own message, still there, and the author may not even learn that much: the channel
        // answers for the message, so a hidden channel never leaks what is in it.
        foreach (var frame in new[]
        {
            Frames.Edit(messageId, "mine again"),
            Frames.Delete(messageId),
            Frames.React(messageId, Fire),
        })
        {
            await author.SendAsync(frame);
            await author.ExpectErrorAsync(ErrorCode.UnknownChannel, fatal: false);
        }

        await owner.QuietAsync();
    }

    [Fact]
    public async Task Frames_naming_a_channel_that_does_not_exist_all_answer_UNKNOWN_CHANNEL()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "kara");
        var owner = party.Client(0);

        // The owner bypasses every permission check, so a refusal here can only be about the id.
        // 0 is the wire's "absent" and no longer means general; a negative id names nothing either.
        foreach (var channelId in new[] { 0L, -1L, UnknownId })
        {
            foreach (var frame in new[]
            {
                Frames.Send("into the void", channelId),
                Frames.MarkRead(channelId, 1),
                Frames.UpdateChannel(channelId, $"ghost {Names.Token()}"),
                Frames.DeleteChannel(channelId),
                Frames.SetOverride(channelId, Frames.RoleOverride(party.EveryoneId, allow: (ulong)Perm.SendMessages)),
            })
            {
                await owner.SendAsync(frame);
                await owner.ExpectErrorAsync(ErrorCode.UnknownChannel, fatal: false);
            }
        }

        await party.Client(1).QuietAsync();
    }

    [Fact]
    public async Task Categories_are_created_renamed_and_deleted_with_their_channels_moved_out()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "lena");
        var owner = party.Client(0);
        var (categoryId, _) = await CreateCategoryAsync(party, $"Section {Names.Token()}");
        var (_, channelId) = await party.CreateChannelAsync(categoryId: categoryId);

        var renamed = $"Renamed {Names.Token()}";
        await owner.SendAsync(Frames.UpdateCategory(categoryId, renamed));
        foreach (var client in party.Clients)
        {
            var category = (await client.ExpectAsync(Kind.CategoryUpserted)).CategoryUpserted.Category;
            Assert.Equal(categoryId, category.Id);
            Assert.Equal(renamed, category.Name);
        }

        // A category's channels outlive it: they move to "no category", which is a sidebar order
        // change and therefore one ChannelOrder for the whole server.
        await owner.SendAsync(Frames.DeleteCategory(categoryId));
        foreach (var client in party.Clients)
        {
            var frames = await client.ExpectSequenceAsync(Kind.CategoryDeleted, Kind.ChannelOrder);
            Assert.Equal(categoryId, frames[0].CategoryDeleted.Id);
            var orphan = Assert.Single(frames[1].ChannelOrder.Positions, position => position.Id == channelId);
            Assert.Equal(0L, orphan.CategoryId);
        }

        foreach (var frame in new[]
        {
            Frames.UpdateCategory(UnknownId, $"nope {Names.Token()}"),
            Frames.DeleteCategory(UnknownId),
            Frames.UpdateCategory(categoryId, renamed),
        })
        {
            await owner.SendAsync(frame);
            await owner.ExpectErrorAsync(ErrorCode.UnknownCategory, fatal: false);
        }

        // The name grammar is checked before the id is looked up, so this is not UNKNOWN_CATEGORY.
        await owner.SendAsync(Frames.UpdateCategory(UnknownId, string.Empty));
        await owner.ExpectErrorAsync(ErrorCode.InvalidName, fatal: false);

        await party.Client(1).QuietAsync();
    }

    [Fact]
    public async Task ReorderChannels_stores_the_full_list_and_broadcasts_it_back_as_ChannelOrder()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "mira");
        var owner = party.Client(0);

        // The owner may view every non-DM channel, so its snapshot is the full list the frame has to
        // carry; a DM has no position and is never in it.
        var existing = owner.Session.Channels.Values
            .Where(channel => channel.Kind != ChannelKind.Dm)
            .Select(channel => Frames.At(channel.Id, channel.CategoryId, channel.Position))
            .ToArray();

        var (_, id) = await party.CreateChannelAsync();
        var moved = Frames.At(id, owner.Session.GeneralCategoryId, 99);
        var wanted = existing.Append(moved).ToArray();

        await owner.SendAsync(Frames.ReorderChannels(wanted));
        foreach (var client in party.Clients)
        {
            var order = (await client.ExpectAsync(Kind.ChannelOrder)).ChannelOrder;
            Assert.Equal(Render(wanted), Render(order.Positions));
        }

        // The full list or nothing: a short one would leave the rest at positions that no longer mean
        // anything, and a repeated id places one channel twice.
        foreach (var bad in new ChannelPosition[][] { wanted[..^1], [], [.. wanted, moved] })
        {
            await owner.SendAsync(Frames.ReorderChannels(bad));
            Assert.Equal("positions", (await owner.ExpectErrorAsync(ErrorCode.InvalidArgument, fatal: false)).Detail);
        }

        // Full length, but one entry names no channel: the ids are checked before the count.
        await owner.SendAsync(Frames.ReorderChannels([.. existing, Frames.At(UnknownId, 0, 0)]));
        await owner.ExpectErrorAsync(ErrorCode.UnknownChannel, fatal: false);

        await party.Client(1).QuietAsync();
    }

    [Fact]
    public async Task ReorderCategories_upserts_only_the_categories_whose_position_changed()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "nina");
        var owner = party.Client(0);

        var before = owner.Session.Categories.Values
            .OrderBy(category => category.Position)
            .ThenBy(category => category.Id)
            .Select(category => (category.Id, category.Position))
            .ToArray();
        var first = await CreateCategoryAsync(party, $"First {Names.Token()}");
        var second = await CreateCategoryAsync(party, $"Second {Names.Token()}");

        // There is no category-order frame: position is the index in the list, and each category that
        // actually moved is broadcast as a CategoryUpserted of its own, in list order.
        var positions = before.Append(first).Append(second).ToDictionary(row => row.Id, row => row.Position);
        var order = before.Select(row => row.Id).Append(second.Id).Append(first.Id).ToArray();
        var expected = order
            .Select((id, index) => (Id: id, Position: index))
            .Where(row => positions[row.Id] != row.Position)
            .ToArray();

        await owner.SendAsync(Frames.ReorderCategories(order));
        foreach (var client in party.Clients)
        {
            foreach (var (categoryId, position) in expected)
            {
                var category = (await client.ExpectAsync(Kind.CategoryUpserted)).CategoryUpserted.Category;
                Assert.Equal(categoryId, category.Id);
                Assert.Equal(position, category.Position);
            }

            await client.QuietAsync();
        }

        foreach (var bad in new long[][] { before.Select(row => row.Id).ToArray(), [], [.. order, first.Id] })
        {
            await owner.SendAsync(Frames.ReorderCategories(bad));
            Assert.Equal("ids", (await owner.ExpectErrorAsync(ErrorCode.InvalidArgument, fatal: false)).Detail);
        }

        await owner.SendAsync(Frames.ReorderCategories([.. before.Select(row => row.Id), first.Id, UnknownId]));
        await owner.ExpectErrorAsync(ErrorCode.UnknownCategory, fatal: false);
    }

    [Fact]
    public async Task Unread_and_mention_counters_are_cleared_by_MarkRead()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol", "bob");
        var alice = party.Account(0);
        var (bob, b1) = (party.Account(2), party.Client(2));
        var general = party.GeneralId;
        var token = Names.Token();

        var plain = $"unread one {token}";
        var mention = $"hey <@{alice.UserId}> unread two {token}";
        long lastId = 0;
        foreach (var (text, mentioned) in new[] { (plain, Array.Empty<long>()), (mention, new[] { alice.UserId }) })
        {
            await b1.SendAsync(Frames.Send(text, general));
            foreach (var client in party.Clients)
            {
                var message = (await client.ExpectAsync(Kind.Message)).Message;
                Assert.Equal(text, message.Text);
                Assert.Equal(general, message.ChannelId);
                Assert.Equal(bob.UserId, message.AuthorId);
                Assert.Equal(mentioned, message.MentionIds.ToArray());
                lastId = message.Id;
            }
        }

        await party.ReconnectAsync(Server, 0);
        var state = party.Client(0).Session.Read(general);
        Assert.Equal((2L, 1L), (state.Unread, state.Mentions));
        Assert.Equal(lastId, state.LastMessageId);

        // MarkRead has no reply, so the pong is what proves the cursor was written before the
        // reconnect below reads it.
        var a1 = party.Client(0);
        await a1.SendAsync(Frames.MarkRead(general, lastId));
        await a1.PingFenceAsync(lastId);

        await party.ReconnectAsync(Server, 0);
        state = party.Client(0).Session.Read(general);
        Assert.Equal((0L, 0L), (state.Unread, state.Mentions));

        // A voice channel holds no messages, so it has no cursor to move either.
        await party.Client(0).SendAsync(Frames.MarkRead(party.GeneralVoiceId, lastId));
        Assert.Equal(
            "channel",
            (await party.Client(0).ExpectErrorAsync(ErrorCode.InvalidArgument, fatal: false)).Detail);
    }

    // CreateCategory by the party's first member, which needs MANAGE_CHANNELS and so the owner party.
    // Every category frame goes to every online member, so this consumes one CategoryUpserted per
    // socket and hands back the id the server assigned with the position it appended it at.
    private static async Task<(long Id, int Position)> CreateCategoryAsync(Party party, string name)
    {
        await party.Client(0).SendAsync(Frames.CreateCategory(name));

        Category? created = null;
        foreach (var client in party.Clients)
        {
            var category = (await client.ExpectAsync(Kind.CategoryUpserted)).CategoryUpserted.Category;
            Assert.Equal(name, category.Name);
            Assert.True(category.Id != 0, $"{client.Label}: the new category has no id");
            Assert.True(
                created is null || created.Id == category.Id,
                $"{client.Label}: category id {category.Id} differs from {created?.Id}");
            created ??= category;
        }

        return (created!.Id, created.Position);
    }

    private static string Render(IEnumerable<ChannelPosition> positions)
        => string.Join(
            " ",
            positions
                .OrderBy(position => position.Id)
                .Select(position => $"{position.Id}:{position.CategoryId}:{position.Position}"));
}
