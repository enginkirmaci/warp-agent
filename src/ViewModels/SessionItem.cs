using CommunityToolkit.Mvvm.ComponentModel;
using WarpAgent.Models;

namespace WarpAgent.ViewModels;

public sealed class SessionItem : ObservableObject
{
    private string _title;
    private DateTime _updated;
    private bool _isPinned;
    private bool _isEditing;
    private bool _isSelected;
    private string _draftTitle = "";

    public SessionItem(string id, string title, DateTime updated)
    {
        Id = id;
        _title = title;
        _updated = updated;
    }

    public string Id { get; }

    public string Title { get => _title; set => SetProperty(ref _title, value); }

    public DateTime Updated
    {
        get => _updated;
        set
        {
            if (SetProperty(ref _updated, value))
                OnPropertyChanged(nameof(UpdatedLabel));
        }
    }

    public bool IsPinned { get => _isPinned; set => SetProperty(ref _isPinned, value); }

    public bool IsEditing { get => _isEditing; set => SetProperty(ref _isEditing, value); }

    public bool IsSelected { get => _isSelected; set => SetProperty(ref _isSelected, value); }

    public string DraftTitle { get => _draftTitle; set => SetProperty(ref _draftTitle, value); }

    public string UpdatedLabel => TimeLabels.Relative(Updated);
}
