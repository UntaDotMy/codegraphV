unit Shapes;
interface
type
  TShape = class(TObject)
  public
    function Area: Double; virtual;
  end;
implementation
function TShape.Area: Double;
begin
  Result := Compute(1, 2);
end;
end.
