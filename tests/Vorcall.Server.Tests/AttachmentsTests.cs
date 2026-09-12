using System.Net;
using Microsoft.Extensions.DependencyInjection;
using Vorcall.Server.Attachments;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class AttachmentsTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Upload_enforces_the_type_the_magic_bytes_the_size_and_the_channel()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "carol");
        var (owner, carol) = (party.Account(0), party.Account(1));
        var general = party.GeneralId;
        var pixel = Png.Pixel();

        var stored = await Uploads.UploadAsync(Server, owner.Access, general, pixel, fileName: "pixel.png");
        Assert.Equal(HttpStatusCode.Created, stored.Status);
        var attachment = stored.As(Attachment.Parser);
        Assert.True(attachment.Id > 0);
        Assert.Equal("pixel.png", attachment.FileName);
        Assert.Equal("image/png", attachment.ContentType);
        Assert.Equal((long)pixel.Length, attachment.Size);

        var bmp = await Uploads.UploadAsync(Server, owner.Access, general, pixel, contentType: "image/bmp");
        Assert.Equal(HttpStatusCode.UnsupportedMediaType, bmp.Status);
        Assert.Equal("unsupported image type", bmp.Detail);

        var notAnImage = await Uploads.UploadAsync(Server, owner.Access, general, "MZ not an image at all"u8.ToArray());
        Assert.Equal(HttpStatusCode.BadRequest, notAnImage.Status);
        Assert.Equal("body is not the declared image type", notAnImage.Detail);

        // The header alone: 9 MiB is above the cap, so the length is refused before any body is read.
        var tooLarge = await Uploads.UploadAsync(Server, owner.Access, general, pixel, declaredLength: 9L * 1024 * 1024);
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, tooLarge.Status);
        Assert.Equal("images must be 8 MiB or smaller", tooLarge.Detail);

        // The channel is a numeric id and has to hold messages for anything to link the upload to.
        foreach (var channel in new[] { "bad!", "0", History.Id(party.GeneralVoiceId) })
        {
            var refused = await Uploads.UploadAsync(Server, owner.Access, channel, pixel);
            Assert.Equal(HttpStatusCode.BadRequest, refused.Status);
            Assert.Equal("channel", refused.Detail);
        }

        // Sight of the channel, then the right to attach in it: two different refusals, each naming
        // the bit that is missing.
        await party.SetMemberOverrideAsync(general, 1, deny: (ulong)Perm.AttachFiles);
        var cannotAttach = await Uploads.UploadAsync(Server, carol.Access, general, pixel);
        Assert.Equal(HttpStatusCode.Forbidden, cannotAttach.Status);
        Assert.Equal(PermNames.Name(Perm.AttachFiles), cannotAttach.Detail);

        // Cleared again: general is shared by the whole collection.
        await party.SetMemberOverrideAsync(general, 1);

        var (_, hiddenId) = await party.CreateChannelAsync();
        await party.Client(0).SendAsync(
            Frames.SetOverride(hiddenId, Frames.MemberOverride(carol.UserId, deny: (ulong)Perm.ViewChannel)));
        await party.Client(1).ExpectChannelDeletedAsync(hiddenId);
        await party.Client(0).ExpectChannelUpsertedAsync(hiddenId);

        var outsider = await Uploads.UploadAsync(Server, carol.Access, hiddenId, pixel);
        Assert.Equal(HttpStatusCode.Forbidden, outsider.Status);
        Assert.Equal(PermNames.Name(Perm.ViewChannel), outsider.Detail);
    }

    [Fact]
    public async Task An_unlinked_upload_belongs_to_its_uploader_alone()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, bob) = (party.Account(0), party.Account(1));
        var pixel = Png.Pixel();
        var attachment = await UploadPixelAsync(alice, party.GeneralId, pixel);

        var stranger = await Uploads.DownloadAsync(Server, bob.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.Forbidden, stranger.Status);

        var owner = await Uploads.DownloadAsync(Server, alice.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.OK, owner.Status);
        Assert.Equal(pixel, owner.Body);
    }

    [Fact]
    public async Task A_linked_upload_is_readable_by_the_channels_viewers_with_ETag_and_Range()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, bob) = (party.Account(0), party.Client(0), party.Account(1));
        var general = party.GeneralId;
        var pixel = Png.Pixel();
        var attachment = await UploadPixelAsync(alice, general, pixel);

        var caption = $"look at this {Names.Token()}";
        await a1.SendAsync(Frames.Send(caption, general, 0, attachment.Id));
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.Message))
        {
            var message = frame.Message;
            Assert.Equal(caption, message.Text);
            Assert.Equal(new[] { attachment.Id }, message.Attachments.Select(item => item.Id).ToArray());
            Assert.Equal("pixel.png", message.Attachments[0].FileName);
            Assert.Equal((long)pixel.Length, message.Attachments[0].Size);
        }

        var whole = await Uploads.DownloadAsync(Server, bob.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.OK, whole.Status);
        Assert.Equal(pixel, whole.Body);
        Assert.Equal($"\"{attachment.Id}-{pixel.Length}\"", whole.Header("ETag"));

        var head = await Uploads.DownloadAsync(Server, bob.Access, attachment.Id, range: "bytes=0-3");
        Assert.Equal(HttpStatusCode.PartialContent, head.Status);
        Assert.Equal(pixel[..4], head.Body);
    }

    [Fact]
    public async Task A_DM_image_is_invisible_outside_the_DM()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, b1) = (party.Account(0), party.Client(0), party.Client(1));
        var (bob, carol, c1) = (party.Account(1), party.Account(2), party.Client(2));
        var dmId = await party.OpenDmAsync(0, 1);

        var attachment = await UploadPixelAsync(alice, dmId, Png.Pixel(), "private.png");
        await a1.SendAsync(Frames.Send(string.Empty, dmId, 0, attachment.Id));
        foreach (var client in new[] { a1, b1 })
        {
            var message = (await client.ExpectAsync(Kind.Message)).Message;
            Assert.Equal(string.Empty, message.Text);
            Assert.Equal(dmId, message.ChannelId);
            Assert.Equal(new[] { attachment.Id }, message.Attachments.Select(item => item.Id).ToArray());
        }

        var outsider = await Uploads.DownloadAsync(Server, carol.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.Forbidden, outsider.Status);
        var member = await Uploads.DownloadAsync(Server, bob.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.OK, member.Status);
        await c1.QuietAsync();
    }

    [Fact]
    public async Task Reuse_or_too_many_attachments_answer_INVALID_ATTACHMENT_and_deleting_the_message_removes_the_file()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1) = (party.Account(0), party.Client(0));
        var general = party.GeneralId;
        var attachment = await UploadPixelAsync(alice, general, Png.Pixel());

        var caption = $"linked {Names.Token()}";
        await a1.SendAsync(Frames.Send(caption, general, 0, attachment.Id));
        var imageMessageId = await party.ExpectMessageEverywhereAsync(caption, general);

        await a1.SendAsync(Frames.Send("again", general, 0, attachment.Id));
        await a1.ExpectErrorAsync(ErrorCode.InvalidAttachment, fatal: false);

        var five = Enumerable.Range(1, 5).Select(offset => attachment.Id + offset).ToArray();
        await a1.SendAsync(Frames.Send("five", general, 0, five));
        await a1.ExpectErrorAsync(ErrorCode.InvalidAttachment, fatal: false);

        await a1.SendAsync(Frames.Delete(imageMessageId));
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.MessageDeleted))
        {
            Assert.Equal(imageMessageId, frame.MessageDeleted.Id);
        }

        var gone = await Uploads.DownloadAsync(Server, alice.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.NotFound, gone.Status);
        var store = Server.Services.GetRequiredService<AttachmentStore>();
        Assert.False(File.Exists(store.PathFor(attachment.Id, attachment.ContentType)));
    }

    private async Task<Attachment> UploadPixelAsync(Account uploader, long channelId, byte[] pixel, string fileName = "pixel.png")
    {
        var response = await Uploads.UploadAsync(Server, uploader.Access, channelId, pixel, fileName: fileName);
        Assert.True(response.Status == HttpStatusCode.Created, $"upload to {channelId}: {(int)response.Status}");
        return response.As(Attachment.Parser);
    }
}
