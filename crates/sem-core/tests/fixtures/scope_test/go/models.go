package models

type Dog struct {
	Name string
}

func NewDog(name string) *Dog {
	return &Dog{Name: name}
}

func (d *Dog) Speak() string {
	return "woof"
}

func (d *Dog) Validate() bool {
	return len(d.Name) > 0
}

type Cat struct {
	Name string
}

func NewCat(name string) *Cat {
	return &Cat{Name: name}
}

func (c *Cat) Speak() string {
	return "meow"
}

func (c *Cat) Validate() bool {
	return len(c.Name) > 0 && len(c.Name) < 50
}

type Shelter struct {
	Animals []string
}

func NewShelter() *Shelter {
	return &Shelter{Animals: []string{}}
}

func (s *Shelter) Add(name string) {
	s.Animals = append(s.Animals, name)
}

func (s *Shelter) Count() int {
	return len(s.Animals)
}
